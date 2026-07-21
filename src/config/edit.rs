//! Schema-reflective editing for the shell's `config show|set|reset` commands.
//!
//! The vocabulary of settable knobs — their names, nesting, types, and default
//! values — is **never retyped here**. It is reflected out of the one schema
//! that already exists: [`Config`](super::Config) / its optfield-generated
//! [`ConfigFile`](super::ConfigFile). A [`template`] materialises every knob
//! into a [`toml::Value`] tree (from
//! [`ConfigFile::effective_defaults`](super::ConfigFile::effective_defaults),
//! which the schema owns), and the three operations navigate/splice that tree —
//! so this module holds no knowledge of any concrete field name:
//!
//! - [`show`] overlays the sparse on-disk [`ConfigFile`] on the template so each
//!   key reports its *effective* current value (a user override, or the built-in
//!   default) paired with that default, for the current/default table.
//! - [`set`] coerces the typed value against the template leaf, splices it into
//!   the sparse file, and **round-trips through `ConfigFile` deserialization** —
//!   so an unknown enum variant, a non-integer, or a negative `usize` is caught
//!   by serde with the same rules that validate the file on load, not by a
//!   hand-maintained validator.
//! - [`reset`] removes the key (pruning a now-empty `[section]`), so it follows
//!   the upgraded default again — sparse persistence preserved.
//!
//! Adding a `Config` field makes it show/set/reset-able automatically; the only
//! per-field code that could drift (a name string, a default) does not exist.

use super::ConfigFile;
use crate::error::Error;
use crate::ui::ConfigRow;
use std::fmt;
use toml::Value;
use toml::de;

/// A dotted config knob path — the addressable key `config show|set|reset` acts
/// on (`color`, `ages.caution_days`).
///
/// A typed wrapper so the path travels as a domain value from the line parser
/// through the edit ops, never confusable with a bare value token (the
/// [typed-identifiers][mem] convention). Construction is unvalidated — any
/// dotted string is a `ConfigPath`; whether it names a real knob is decided by
/// the edit ops against the schema, exactly as a [`PkgTarget`](crate::names::PkgTarget)
/// is validated by resolution, not by its constructor.
///
/// [mem]: crate::config
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConfigPath(String);

impl ConfigPath {
    /// The path as its dotted string — for tree navigation and prefix matching.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ConfigPath {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for ConfigPath {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl AsRef<str> for ConfigPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConfigPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a `config` edit was rejected — path or value validation, kept structured
/// so the shell can prefix it uniformly and the tests assert on the variant.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// No such knob (the dotted path matches nothing in the schema).
    Unknown(String),
    /// The path names a whole `[section]` (e.g. `ages`), not one setting.
    Section(String),
    /// The value doesn't fit the knob's TOML type (wrong token count, or a
    /// non-boolean/non-integer where one is required).
    WrongType {
        /// The knob the bad value was for.
        path: String,
        /// What a valid value looks like ("an integer", "a boolean …").
        expected: &'static str,
    },
    /// serde rejected the value when re-parsing the file — an unknown enum
    /// variant, a negative `usize`, etc. Carries serde's own message (which
    /// helpfully lists the valid variants for an enum knob).
    Invalid {
        /// The knob the value was for.
        path: String,
        /// serde's rejection message (first line, trimmed).
        message: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(p) => {
                write!(
                    f,
                    "unknown config key `{p}` — `config show` lists every key"
                )
            }
            Self::Section(p) => write!(
                f,
                "`{p}` is a section, not a setting — try `{p}.<key>` (see `config show {p}`)"
            ),
            Self::WrongType { path, expected } => write!(f, "`{path}` expects {expected}"),
            Self::Invalid { path, message } => write!(f, "`{path}`: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<ConfigError> for Error {
    fn from(e: ConfigError) -> Self {
        Self::other(e.to_string())
    }
}

/// The result of a `config set`/`reset`: the new sparse [`ConfigFile`] to
/// persist plus a one-line summary of the change for the caller to print.
///
/// A named pair rather than an anonymous `(ConfigFile, String)`, so `set`/
/// `reset` and their callers read `.file`/`.summary` instead of `.0`/`.1`.
#[derive(Debug)]
pub struct ConfigEdit {
    /// The updated on-disk schema — the caller persists it (e.g. through
    /// [`ConfigHandle::update`](super::ConfigHandle::update)).
    pub file: ConfigFile,
    /// A human-readable one-line summary of what changed.
    pub summary: String,
}

/// Every settable knob's dotted path, sorted — the completion + "did you look
/// here" surface. Derived from the schema, so it can't fall out of sync.
pub fn paths() -> Vec<ConfigPath> {
    let template = template();
    let mut paths: Vec<ConfigPath> = leaves(&template)
        .into_iter()
        .map(|(k, _)| ConfigPath(k))
        .collect();
    paths.sort();
    paths
}

/// `config show [path]`: one [`ConfigRow`] per knob (current + default value).
///
/// Lists all knobs, one knob, or every knob under a `[section]`. Each row pairs
/// the knob's *effective* current value (a user override, or the default) with
/// its built-in default; the renderer highlights the ones that differ.
pub fn show(file: &ConfigFile, path: Option<&ConfigPath>) -> Result<Vec<ConfigRow>, ConfigError> {
    let template = template();
    let file_tree = to_value(file);
    // The (path, default-value) pairs to render: everything, one leaf, or a
    // section's leaves.
    let selected: Vec<(String, &Value)> = match path {
        None => leaves(&template),
        Some(p) => match node(&template, p.as_str()) {
            None => return Err(ConfigError::Unknown(p.as_str().to_owned())),
            Some(Value::Table(section)) => {
                let mut out = Vec::new();
                flatten(section, p.as_str(), &mut out);
                out
            }
            Some(leaf) => vec![(p.as_str().to_owned(), leaf)],
        },
    };
    Ok(selected
        .into_iter()
        .map(|(key, default)| {
            // Current = the file's override if any, else the default itself.
            let current = node(&file_tree, &key).unwrap_or(default);
            ConfigRow {
                current: render_value(current),
                default: render_value(default),
                path: key,
            }
        })
        .collect())
}

/// `config set <path> <value…>`: validate the knob + value, returning the new
/// sparse file and a summary.
///
/// Returns the new [`ConfigFile`] (the caller persists it) plus a one-line
/// summary. The file is edited in TOML space and re-parsed, so serde validates
/// the value exactly as it would on load.
pub fn set(
    file: &ConfigFile,
    path: &ConfigPath,
    value: &[String],
) -> Result<ConfigEdit, ConfigError> {
    let template = template();
    let key = path.as_str();
    let leaf = require_leaf(&template, key)?;
    let coerced = coerce(key, value, leaf)?;
    let mut tree = to_value(file);
    let old = node(&tree, key).map_or_else(|| render_value(leaf), render_value);
    insert_path(
        tree.as_table_mut()
            .expect("a ConfigFile serializes to a table"),
        key,
        coerced.clone(),
    );
    let new_file: ConfigFile = tree.try_into().map_err(|e| invalid(key, &e))?;
    Ok(ConfigEdit {
        file: new_file,
        summary: format!("{path} = {}  (was {old})", render_value(&coerced)),
    })
}

/// `config reset <path>`: drop the user's override so the knob follows the
/// built-in default again, pruning a now-empty `[section]`. Returns the new
/// sparse file plus a summary.
pub fn reset(file: &ConfigFile, path: &ConfigPath) -> Result<ConfigEdit, ConfigError> {
    let template = template();
    let key = path.as_str();
    let default = render_value(require_leaf(&template, key)?);
    let mut tree = to_value(file);
    let was_set = remove_path(
        tree.as_table_mut()
            .expect("a ConfigFile serializes to a table"),
        key,
    );
    let new_file: ConfigFile = tree.try_into().map_err(|e| invalid(key, &e))?;
    let summary = if was_set {
        format!("{path} reset to {default}  (default)")
    } else {
        format!("{path} is already at its default ({default})")
    };
    Ok(ConfigEdit {
        file: new_file,
        summary,
    })
}

/// The complete schema as a TOML tree with every knob materialised to its
/// effective default — the reflection surface all three ops navigate. The
/// schema owns the materialization ([`ConfigFile::effective_defaults`]); this
/// module only turns it into a navigable tree, and so stays free of any
/// knowledge of concrete field names.
fn template() -> Value {
    to_value(&ConfigFile::effective_defaults())
}

/// Serialize a [`ConfigFile`] to a TOML [`Value::Table`]. Infallible: the schema
/// holds only TOML-representable types.
fn to_value(file: &ConfigFile) -> Value {
    Value::try_from(file).expect("a ConfigFile always serializes to TOML")
}

/// Resolve a dotted path to the leaf a `set`/`reset` acts on, rejecting a
/// missing knob and a whole-section path.
fn require_leaf<'a>(template: &'a Value, path: &str) -> Result<&'a Value, ConfigError> {
    match node(template, path) {
        None => Err(ConfigError::Unknown(path.to_owned())),
        Some(Value::Table(_)) => Err(ConfigError::Section(path.to_owned())),
        Some(leaf) => Ok(leaf),
    }
}

/// Follow a dotted path into a TOML tree.
fn node<'a>(tree: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = tree;
    for part in path.split('.') {
        cur = cur.as_table()?.get(part)?;
    }
    Some(cur)
}

/// Splice `value` in at a dotted path, creating intermediate `[section]` tables
/// as needed (the template guarantees the path shape, so an existing non-table
/// on the way is never valid input and is simply overwritten).
fn insert_path(table: &mut toml::Table, path: &str, value: Value) {
    let Some((head, rest)) = path.split_once('.') else {
        table.insert(path.to_owned(), value);
        return;
    };
    if !table.get(head).is_some_and(Value::is_table) {
        table.insert(head.to_owned(), Value::Table(toml::Table::new()));
    }
    if let Some(Value::Table(sub)) = table.get_mut(head) {
        insert_path(sub, rest, value);
    }
}

/// Remove a dotted path, pruning a `[section]` left empty by the removal.
/// Returns whether anything was actually there to remove.
fn remove_path(table: &mut toml::Table, path: &str) -> bool {
    let Some((head, rest)) = path.split_once('.') else {
        return table.remove(path).is_some();
    };
    let removed = if let Some(Value::Table(sub)) = table.get_mut(head) {
        remove_path(sub, rest)
    } else {
        false
    };
    if let Some(Value::Table(sub)) = table.get(head)
        && sub.is_empty()
    {
        table.remove(head);
    }
    removed
}

/// Enumerate the leaf (non-table) values of a TOML tree with their dotted paths.
fn leaves(tree: &Value) -> Vec<(String, &Value)> {
    let mut out = Vec::new();
    if let Value::Table(table) = tree {
        flatten(table, "", &mut out);
    }
    out
}

/// Recursive worker for [`leaves`]: arrays count as leaves, tables recurse.
fn flatten<'a>(table: &'a toml::Table, prefix: &str, out: &mut Vec<(String, &'a Value)>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            Value::Table(sub) => flatten(sub, &path, out),
            _ => out.push((path, value)),
        }
    }
}

/// Coerce the raw `value` tokens into a TOML value of the knob's type. Type
/// *shape* is enforced here (token count, boolean/integer syntax); the finer
/// domain (which enum variants, `usize` non-negativity) is left to the
/// [`set`] re-parse so serde stays the one validator.
fn coerce(path: &str, raw: &[String], leaf: &Value) -> Result<Value, ConfigError> {
    match leaf {
        Value::Boolean(_) => match single(path, raw, "a boolean (true or false)")?
            .to_ascii_lowercase()
            .as_str()
        {
            "true" => Ok(Value::Boolean(true)),
            "false" => Ok(Value::Boolean(false)),
            _ => Err(ConfigError::WrongType {
                path: path.to_owned(),
                expected: "a boolean (true or false)",
            }),
        },
        Value::Integer(_) => single(path, raw, "an integer")?
            .parse::<i64>()
            .map(Value::Integer)
            .map_err(|_err| ConfigError::WrongType {
                path: path.to_owned(),
                expected: "an integer",
            }),
        // Strings cover free-form knobs and the enum knobs alike (an enum
        // serializes to its variant string); a bad enum variant is caught by
        // the `set` re-parse, which reports the valid ones.
        Value::String(_) => Ok(Value::String(
            single(path, raw, "a single value")?.to_owned(),
        )),
        // List knobs (e.g. `makepkg_args`) take every token.
        Value::Array(_) => Ok(Value::Array(
            raw.iter().map(|s| Value::String(s.clone())).collect(),
        )),
        _ => Err(ConfigError::WrongType {
            path: path.to_owned(),
            expected: "a supported value",
        }),
    }
}

/// Require exactly one value token for a scalar knob.
fn single<'a>(
    path: &str,
    raw: &'a [String],
    expected: &'static str,
) -> Result<&'a str, ConfigError> {
    match raw {
        [one] => Ok(one),
        _ => Err(ConfigError::WrongType {
            path: path.to_owned(),
            expected,
        }),
    }
}

/// A human-friendly rendering of a knob value: scalars unquoted, a list as
/// space-joined tokens — copy-readable, not TOML-literal.
fn render_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Integer(i) => i.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Datetime(d) => d.to_string(),
        Value::Array(items) if items.is_empty() => "(none)".to_owned(),
        Value::Array(items) => items.iter().map(render_value).collect::<Vec<_>>().join(" "),
        Value::Table(_) => "{…}".to_owned(),
    }
}

/// Wrap a `ConfigFile` re-parse failure as an [`ConfigError::Invalid`], keeping
/// serde's first message line (which names the valid enum variants).
fn invalid(path: &str, err: &de::Error) -> ConfigError {
    let message = err
        .to_string()
        .lines()
        .next()
        .unwrap_or("invalid value")
        .trim()
        .to_owned();
    ConfigError::Invalid {
        path: path.to_owned(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assert_contains;

    /// Parse a sparse file from TOML text — the fixture shape the ops act on.
    fn file(text: &str) -> ConfigFile {
        toml::from_str(text).expect("fixture parses")
    }

    fn vals(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|s| (*s).to_owned()).collect()
    }

    /// A [`ConfigPath`] from a literal — the ops take the domain type.
    fn p(path: &str) -> ConfigPath {
        ConfigPath::from(path)
    }

    /// The path universe is reflected from the schema: a representative flat
    /// knob, an enum knob, and the nested `[ages]` leaves all appear, and the
    /// whole-section name does not (only leaves are settable).
    #[test]
    fn paths_are_reflected_from_the_schema() {
        let paths = paths();
        for expected in [
            "aur",
            "color",
            "privilege_escalator",
            "search_layout",
            "makepkg_args",
            "ages.caution_days",
            "ages.stale_days",
        ] {
            assert!(
                paths.iter().any(|p| p.as_str() == expected),
                "missing `{expected}`"
            );
        }
        assert!(
            !paths.iter().any(|p| p.as_str() == "ages"),
            "the bare section name is not a settable path"
        );
    }

    /// `show` pairs each knob's effective *current* value with its default: an
    /// overridden knob's two columns differ, an unset one's match.
    #[test]
    fn show_pairs_current_and_default() {
        let f = file("color = \"never\"\n");
        let rows = show(&f, None).unwrap();
        let by = |key: &str| rows.iter().find(|r| r.path == key).expect("knob present");
        // The override: current differs from default.
        let color = by("color");
        assert_eq!(
            (color.current.as_str(), color.default.as_str()),
            ("never", "auto")
        );
        // An unset flat knob: current == default.
        let aur = by("aur");
        assert_eq!(
            (aur.current.as_str(), aur.default.as_str()),
            ("true", "true")
        );
        // A nested default resolves from the age thresholds, current == default.
        let caution = by("ages.caution_days");
        assert_eq!(
            (caution.current.as_str(), caution.default.as_str()),
            ("2", "2")
        );
    }

    /// `show <section>` lists just that section's leaves.
    #[test]
    fn show_a_section_lists_its_leaves() {
        let rows = show(&ConfigFile::default(), Some(&p("ages"))).unwrap();
        assert_eq!(rows.len(), 3, "the three age bands");
        assert!(rows.iter().all(|r| r.path.starts_with("ages.")));
    }

    /// `show <unknown>` is a clean error, not a panic or empty list.
    #[test]
    fn show_unknown_path_errors() {
        assert_eq!(
            show(&ConfigFile::default(), Some(&p("nope"))).unwrap_err(),
            ConfigError::Unknown("nope".to_owned())
        );
    }

    /// A boolean set validates its value and round-trips into the file.
    #[test]
    fn set_bool_persists_and_reports_prior() {
        let ConfigEdit { file: new, summary } =
            set(&ConfigFile::default(), &p("aur"), &vals(&["false"])).unwrap();
        assert!(!new.resolve().aur);
        assert_contains!(summary, "aur = false");
        assert_contains!(summary, "was true");
        // A non-boolean is rejected before it can reach the file.
        assert_eq!(
            set(&ConfigFile::default(), &p("aur"), &vals(&["maybe"])).unwrap_err(),
            ConfigError::WrongType {
                path: "aur".to_owned(),
                expected: "a boolean (true or false)",
            }
        );
    }

    /// An enum knob is validated by the re-parse, and the error lists the valid
    /// variants (serde's own message) rather than a hand-maintained list.
    #[test]
    fn set_enum_rejects_unknown_variant_with_valid_ones() {
        assert!(
            set(
                &ConfigFile::default(),
                &p("search_layout"),
                &vals(&["double"])
            )
            .is_ok()
        );
        let err = set(
            &ConfigFile::default(),
            &p("search_layout"),
            &vals(&["triple"]),
        )
        .unwrap_err();
        match err {
            ConfigError::Invalid { path, message } => {
                assert_eq!(path, "search_layout");
                assert_contains!(message, "single");
                assert_contains!(message, "double");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
        // The newly-typed `color`/`privilege_escalator` enums validate too.
        assert!(set(&ConfigFile::default(), &p("color"), &vals(&["never"])).is_ok());
        assert!(set(&ConfigFile::default(), &p("color"), &vals(&["nver"])).is_err());
        assert!(
            set(
                &ConfigFile::default(),
                &p("privilege_escalator"),
                &vals(&["doas"])
            )
            .is_ok()
        );
        assert!(
            set(
                &ConfigFile::default(),
                &p("privilege_escalator"),
                &vals(&["pkexec"])
            )
            .is_err()
        );
    }

    /// An integer knob rejects non-integers and negative values (the latter via
    /// the `usize` re-parse).
    #[test]
    fn set_integer_validates_type_and_domain() {
        let ConfigEdit { file: new, .. } =
            set(&ConfigFile::default(), &p("index_threads"), &vals(&["8"])).unwrap();
        assert_eq!(new.resolve().index_threads, 8);
        assert!(matches!(
            set(&ConfigFile::default(), &p("index_threads"), &vals(&["abc"])),
            Err(ConfigError::WrongType { .. })
        ));
        assert!(matches!(
            set(&ConfigFile::default(), &p("index_threads"), &vals(&["-1"])),
            Err(ConfigError::Invalid { .. })
        ));
    }

    /// A scalar knob rejects a multi-token value; a list knob takes them all.
    #[test]
    fn set_scalar_vs_list_arity() {
        assert!(matches!(
            set(
                &ConfigFile::default(),
                &p("color"),
                &vals(&["never", "always"])
            ),
            Err(ConfigError::WrongType { .. })
        ));
        let ConfigEdit { file: new, .. } = set(
            &ConfigFile::default(),
            &p("makepkg_args"),
            &vals(&["-d", "--needed"]),
        )
        .unwrap();
        assert_eq!(new.resolve().makepkg_args, vec!["-d", "--needed"]);
    }

    /// Setting a nested knob creates the `[ages]` section and leaves the user's
    /// other keys untouched (sparse persistence).
    #[test]
    fn set_nested_key_is_sparse() {
        let f = file("index_threads = 3\n");
        let ConfigEdit { file: new, .. } = set(&f, &p("ages.caution_days"), &vals(&["7"])).unwrap();
        let text = toml::to_string(&new).unwrap();
        assert_contains!(text, "caution_days = 7");
        assert_contains!(text, "index_threads = 3");
        // Only the one age band materialises; the others still follow defaults.
        assert!(!text.contains("fresh_days"), "stayed sparse: {text}");
        assert_eq!(new.resolve().ages.caution_days, Some(7));
    }

    /// Setting a whole section is refused with a pointer at the concrete key.
    #[test]
    fn set_section_path_is_refused() {
        assert_eq!(
            set(&ConfigFile::default(), &p("ages"), &vals(&["7"])).unwrap_err(),
            ConfigError::Section("ages".to_owned())
        );
    }

    /// `reset` drops an override and, for the last key in a section, prunes the
    /// now-empty section so nothing default materialises on disk.
    #[test]
    fn reset_clears_override_and_prunes_empty_section() {
        let f = file("[ages]\ncaution_days = 7\n");
        let ConfigEdit { file: new, summary } = reset(&f, &p("ages.caution_days")).unwrap();
        assert_contains!(summary, "reset to 2");
        let text = toml::to_string(&new).unwrap();
        assert!(
            !text.contains("ages") && !text.contains("caution_days"),
            "the emptied section is pruned: {text:?}"
        );
        assert_eq!(new.resolve().ages.caution_days, None);
    }

    /// Resetting an already-default knob is a friendly no-op that says so.
    #[test]
    fn reset_unset_knob_reports_already_default() {
        let ConfigEdit { summary, .. } = reset(&ConfigFile::default(), &p("color")).unwrap();
        assert_contains!(summary, "already at its default");
        assert_contains!(summary, "auto");
    }
}
