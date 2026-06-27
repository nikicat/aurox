//! Command vocabulary for the interactive shell + the line parser.
//!
//! Words-only (no pacman-letter clusters, no clap): a line is tokenized with
//! `shell_words` and the first token is matched against a verb. In phase 1 the
//! argument-bearing verbs keep their args as raw strings; later phases parse
//! them into `Selector`s (numbers / ranges / names / globs).

/// One parsed shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `search <terms…>` — find packages across repos + AUR.
    Search(Vec<String>),
    /// `info <pkg…>` — show package details.
    Info(Vec<String>),
    /// `add <pkg…>` — stage packages to install.
    Add(Vec<String>),
    /// `drop <pkg…>` — unstage packages from the cart.
    Drop(Vec<String>),
    /// `remove <pkg…>` — stage packages to uninstall.
    Remove(Vec<String>),
    /// `upgrade [pkg…]` — stage available upgrades (all, or the matching subset).
    Upgrade(Vec<String>),
    /// `review <pkg…>` — view a PKGBUILD/diff and approve it.
    Review(Vec<String>),
    /// `show` — preview the staged transaction.
    Show,
    /// `apply` — build + install the staged transaction.
    Apply,
    /// `clear` — empty the cart.
    Clear,
    /// `refresh` — re-fetch the AUR mirror + index.
    Refresh,
    /// `help [command]` — command list (optional per-command topic).
    Help(Option<String>),
    /// `quit` / `exit` / Ctrl-D — leave the shell.
    Quit,
    /// Blank or whitespace-only line — a no-op.
    Empty,
    /// First token didn't match any known verb; carries the verb as typed.
    Unknown(String),
    /// The line couldn't be tokenized (e.g. an unbalanced quote); carries the
    /// tokenizer's message.
    Syntax(String),
}

impl Command {
    /// Canonical verb name, for diagnostics and the phase-1 stub messages.
    pub const fn verb(&self) -> &'static str {
        match self {
            Self::Search(_) => "search",
            Self::Info(_) => "info",
            Self::Add(_) => "add",
            Self::Drop(_) => "drop",
            Self::Remove(_) => "remove",
            Self::Upgrade(_) => "upgrade",
            Self::Review(_) => "review",
            Self::Show => "show",
            Self::Apply => "apply",
            Self::Clear => "clear",
            Self::Refresh => "refresh",
            Self::Help(_) => "help",
            Self::Quit => "quit",
            Self::Empty => "",
            Self::Unknown(_) | Self::Syntax(_) => "?",
        }
    }
}

/// Parse one input line into a [`Command`].
///
/// Never fails: tokenizer errors become [`Command::Syntax`] and an unrecognized
/// verb becomes [`Command::Unknown`], so a bad line reports and the REPL keeps
/// going rather than aborting the session.
pub fn parse(line: &str) -> Command {
    let tokens = match shell_words::split(line) {
        Ok(t) => t,
        Err(e) => return Command::Syntax(e.to_string()),
    };
    let Some((verb, args)) = tokens.split_first() else {
        return Command::Empty;
    };
    let args = args.to_vec();
    match verb.to_ascii_lowercase().as_str() {
        "search" => Command::Search(args),
        "info" => Command::Info(args),
        "add" | "install" => Command::Add(args),
        "drop" | "unstage" => Command::Drop(args),
        "remove" | "uninstall" | "rm" => Command::Remove(args),
        "upgrade" | "up" => Command::Upgrade(args),
        "review" => Command::Review(args),
        "show" | "status" => Command::Show,
        "apply" | "commit" => Command::Apply,
        "clear" => Command::Clear,
        "refresh" => Command::Refresh,
        "help" | "?" => Command::Help(args.into_iter().next()),
        "quit" | "exit" | "q" => Command::Quit,
        _ => Command::Unknown(verb.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_verb_and_args() {
        assert_eq!(parse("search foo bar"), Command::Search(v(&["foo", "bar"])));
    }

    #[test]
    fn empty_and_whitespace_are_empty() {
        assert_eq!(parse(""), Command::Empty);
        assert_eq!(parse("   \t "), Command::Empty);
    }

    #[test]
    fn verb_is_case_insensitive() {
        assert_eq!(parse("SEARCH x"), Command::Search(v(&["x"])));
        assert_eq!(parse("Quit"), Command::Quit);
    }

    #[test]
    fn aliases_map_to_canonical() {
        assert_eq!(parse("install x"), Command::Add(v(&["x"])));
        assert_eq!(parse("up"), Command::Upgrade(v(&[])));
        assert_eq!(parse("commit"), Command::Apply);
        assert_eq!(parse("status"), Command::Show);
        assert_eq!(parse("exit"), Command::Quit);
        assert_eq!(parse("q"), Command::Quit);
    }

    #[test]
    fn quoting_groups_tokens() {
        assert_eq!(
            parse(r#"add "name with space" other"#),
            Command::Add(v(&["name with space", "other"]))
        );
    }

    #[test]
    fn unterminated_quote_is_syntax_error() {
        assert!(matches!(parse("add \"x"), Command::Syntax(_)));
    }

    #[test]
    fn unknown_verb_carries_token() {
        assert_eq!(parse("frobnicate x"), Command::Unknown("frobnicate".into()));
    }

    #[test]
    fn help_takes_optional_topic() {
        assert_eq!(parse("help"), Command::Help(None));
        assert_eq!(parse("help add"), Command::Help(Some("add".into())));
    }

    #[test]
    fn arg_only_verbs_ignore_extra_tokens() {
        // `show`/`apply`/`clear`/`refresh` take no args in phase 1.
        assert_eq!(parse("show now please"), Command::Show);
        assert_eq!(parse("apply"), Command::Apply);
    }
}
