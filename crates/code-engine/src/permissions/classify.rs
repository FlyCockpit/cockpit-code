//! Deterministic shell-command classifier.
//!
//! Parses a proposed shell-command string into a real bash-grammar AST
//! (`brush-parser`) — **not** a substring scan — and decomposes it into
//! the set of *simple commands* it would actually run, with executable
//! identities, wrapper flags and risk metadata for permission decisions.
//!
//! ## Why a parser, not a scan
//!
//! `echo "a && b"` is **one** simple command — the `&&` lives inside a
//! quoted word, not between two commands. A substring scan for `&&`
//! would wrongly split it; the AST keeps the quoted `&&` as part of a
//! single [`brush_parser::ast::Word`] value (raw text, quotes included),
//! so it never reads as a separator. Same for `|`, `;`, `()`, `$(...)`
//! inside quotes. We parse in `sh_mode` because `bash.rs` executes every
//! command via `sh -c <command>` — classification matches execution.
//!
//! ## The fundamental limit (documented on purpose)
//!
//! Static analysis bounds **syntax, not behavior**. A wrapper like
//! `bash -c "<script>"`, `eval "$x"`, or `xargs rm` carries a *dynamic*
//! command string the grammar cannot inspect — the inner program is data
//! at parse time. The classifier flags these as wrappers so permission
//! decisions can account for the behavior hidden by the command string. This is the same reason
//! command-substitution `$(...)` and process-substitution force a prompt:
//! the substituted program isn't statically knowable.

use std::io::Cursor;

use brush_parser::ast::{
    self, Command, CommandPrefixOrSuffixItem, CompoundCommand, IoFileRedirectTarget, IoRedirect,
    Pipeline, SimpleCommand, SourceLocation,
};
use brush_parser::{Parser, ParserOptions};

/// Commands whose first argument is itself an arbitrary program or
/// command string the parser cannot inspect. Flagged so the store
/// refuses to persist grants for them: they re-prompt every run.
///
/// `argv[0]` match only. `sudo`/`env`/`timeout`/`nice` are *prefix*
/// wrappers — they run whatever command follows with altered
/// privilege/environment/limits, so a grant for the wrapper would
/// silently cover anything chained behind it.
const WRAPPER_COMMANDS: &[&str] = &[
    "bash",
    "sh",
    "zsh",
    "dash",
    "ksh",
    "fish", // `-c "<script>"`
    "eval",
    "source",
    ".", // evaluate a dynamic string / file
    "command",
    "exec",
    "builtin", // shell builtins that dispatch or alter another command
    "xargs",   // build + run a command from stdin
    "find",    // `-exec` runs an arbitrary command per match
    "ssh",     // runs a remote command string
    "sudo",
    "doas", // privilege escalation prefix
    "env",  // sets env then execs an arbitrary command
    "timeout",
    "nice",
    "nohup",
    "stdbuf",
    "setsid", // exec-prefix wrappers
    "time",
    "flock",
    "run-parts", // wrappers whose operands select commands/scripts
    "watch",     // re-runs an arbitrary command on an interval
];

/// Commands that execute under a changed root/namespace or multiplex
/// applet dispatch. They are once-only even when the executable word is
/// static because a broad grant hides privileged execution context.
const PRIVILEGED_NON_PERSISTABLE_COMMANDS: &[&str] = &["chroot", "unshare", "busybox"];

/// One simple command extracted from a (possibly compound) command
/// string, with everything the approval store needs to decide it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommandInfo {
    /// `argv[0]` — the program token, verbatim for UI/display (`git`, `"rm"`).
    pub program: String,
    /// Static executable identity after quote/escape reduction, used for
    /// permission decisions instead of raw shell syntax.
    pub normalized_program: String,
    /// First subcommand token, if any (`pr` for `gh pr create`). `None`
    /// for no-subcommand commands (`ls`, `./script`).
    pub subcommand: Option<String>,
    /// Static suffix argv words after `argv[0]`, preserving source order.
    /// Risk classifiers use them to raise the tier for dangerous flags.
    pub args: Vec<String>,
    /// Whether this command is a wrapper/eval that hides behavior the
    /// parser can't inspect. Wrappers are never persistable.
    pub wrapper: bool,
    /// Whether an option value can name executable behavior. These commands
    /// require review even when the executable name is recognized.
    pub execution_bearing_option: bool,
    /// Structured risk/effect metadata used by the approval policy and
    /// prompt. This is conservative static analysis: only literal operands
    /// become affected paths; dynamic/globbed operands become risk reasons.
    pub risk: RiskMetadata,
    /// Char range `[start, end)` (0-based, end-exclusive) of this simple
    /// command within the original command string, from `brush-parser`'s
    /// AST source spans. Used by the approval dialog to highlight the
    /// constituent this prompt decides inside the full verbatim command.
    /// `None` when the parser did not place this command (no span info on
    /// the node — e.g. a degenerate construct); the dialog then falls back
    /// to a step indicator without an inline highlight.
    pub span: Option<CharSpan>,
}

/// A 0-based, end-exclusive **char** range into the original command
/// string. Char-indexed (matching `brush-parser`'s `SourcePosition.index`,
/// which counts chars) so multi-byte input slices correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharSpan {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RiskMetadata {
    pub tier: RiskTier,
    pub reasons: Vec<String>,
    pub affected_paths: Vec<String>,
    pub native_tool_hints: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum RiskTier {
    #[default]
    Ordinary,
    Mutating,
    Destructive,
    Privileged,
    Dynamic,
}

impl RiskTier {
    pub fn as_str(self) -> &'static str {
        match self {
            RiskTier::Ordinary => "ordinary",
            RiskTier::Mutating => "mutating",
            RiskTier::Destructive => "destructive",
            RiskTier::Privileged => "privileged",
            RiskTier::Dynamic => "dynamic",
        }
    }
}

/// Outcome of classifying a command string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// Parsed into one or more simple commands. `simple_commands` lists
    /// each, in source order. A string that decomposes to a single
    /// element was a single simple command; more than one means it was
    /// compound (`&&`, `|`, `;`, subshell, …) and every constituent must
    /// be granted independently.
    Parsed {
        simple_commands: Vec<SimpleCommandInfo>,
        /// True if the source was compound (chained/piped/grouped/
        /// backgrounded/redirected/substituted) rather than one bare
        /// simple command. Surfaced for callers that want to message it.
        compound: bool,
    },
    /// Valid shell grammar that executes effects the decomposer cannot
    /// attribute to any runnable simple command, so the old
    /// "no simple command ⇒ nothing happens" reading would be wrong:
    /// a redirect-only line (`>important-file`) opens or truncates a
    /// file, an assignment-only line (`PATH=/evil/bin`,
    /// `PROMPT_COMMAND=…`, `FOO=bar`) mutates interactive-shell state
    /// even without substitution, an assignment carrying command
    /// substitution (`EVIL=$(curl … | sh)`) runs the substituted
    /// pipeline, and other commandless structure (`{ >out; }`, `coproc`,
    /// function definitions) keeps command-shaped syntax out of prose.
    /// Nothing here can ever be auto-run: approval consumers treat it
    /// exactly like [`Classification::Unparseable`] — deny/prompt, never
    /// grant.
    EffectsOnly,
    /// Empty, whitespace-only, or comment-only input — nothing to run,
    /// treated as not-granted by the store (never silently auto-allowed).
    /// Assignments are not empty: they mutate shell state.
    Empty,
    /// The string could not be parsed as a shell program. Treated as
    /// not-granted; the caller surfaces the error and prompts.
    Unparseable(String),
}

impl Classification {
    /// The simple commands, or an empty slice for
    /// `EffectsOnly`/`Empty`/`Unparseable`.
    pub fn simple_commands(&self) -> &[SimpleCommandInfo] {
        match self {
            Classification::Parsed {
                simple_commands, ..
            } => simple_commands,
            _ => &[],
        }
    }
}

/// Classify a proposed shell-command string. Pure and synchronous —
/// the standalone-testable core of the subsystem.
pub fn classify(command: &str) -> Classification {
    if command.trim().is_empty() {
        return Classification::Empty;
    }

    // Match the `sh -c <command>` grammar used by the bash tool.
    let opts = ParserOptions {
        sh_mode: true,
        ..ParserOptions::default()
    };
    let mut parser = Parser::new(Cursor::new(command.as_bytes().to_vec()), &opts);

    let program = match parser.parse_program() {
        Ok(p) => p,
        Err(e) => return Classification::Unparseable(e.to_string()),
    };

    // A parse that yields no complete commands (e.g. only comments) has
    // nothing to run.
    if program.complete_commands.is_empty() {
        return Classification::Empty;
    }

    let mut acc = Decomposer::default();
    for complete_command in &program.complete_commands {
        acc.walk_compound_list(complete_command);
    }

    if acc.simple_commands.is_empty() {
        // No runnable simple command — but "nothing to run" is not "no
        // effect": a redirect-only line (`>important-file`) opens or
        // truncates a file when the receiver executes it, an
        // assignment-only line (`PATH=/evil/bin`, `PROMPT_COMMAND=…`)
        // mutates interactive-shell state (and can run later, at prompt
        // render or via a redirected PATH), and structure the decomposer
        // could not attribute to a simple command (substitution-bearing
        // assignments, commandless groups, `coproc`) keeps shell syntax
        // with real effects out of the benign `Empty` bucket. Only a
        // genuinely effect-free line (whitespace, comments) stays
        // `Empty`.
        if acc.effects_only || acc.compound {
            return Classification::EffectsOnly;
        }
        return Classification::Empty;
    }

    Classification::Parsed {
        compound: acc.compound,
        simple_commands: acc.simple_commands,
    }
}

/// Accumulates simple commands while walking the AST, tracking whether
/// the source turned out to be compound.
#[derive(Default)]
struct Decomposer {
    simple_commands: Vec<SimpleCommandInfo>,
    compound: bool,
    /// True when any I/O redirect or assignment was noted anywhere in
    /// the program. Redirects are filesystem/child-process effects even
    /// on a line with no program word; assignments mutate shell state
    /// (`PATH`, `PROMPT_COMMAND`, …) even without substitution. Either
    /// must keep such lines out of the benign `Empty` bucket.
    effects_only: bool,
}

impl Decomposer {
    /// Walk a `CompoundList` — a `;`/`&`-separated sequence of and-or
    /// lists. More than one item, or any async (`&`) item, is compound.
    fn walk_compound_list(&mut self, list: &ast::CompoundList) {
        if list.0.len() > 1 {
            self.compound = true;
        }
        for item in &list.0 {
            if matches!(item.1, ast::SeparatorOperator::Async) {
                self.compound = true;
            }
            self.walk_and_or_list(&item.0);
        }
    }

    /// Walk an and-or list — pipelines joined by `&&`/`||`. More than one
    /// pipeline is compound.
    fn walk_and_or_list(&mut self, list: &ast::AndOrList) {
        if !list.additional.is_empty() {
            self.compound = true;
        }
        for (_op, pipeline) in list {
            self.walk_pipeline(pipeline);
        }
    }

    /// Walk a pipeline. More than one command in the pipe is compound.
    fn walk_pipeline(&mut self, pipeline: &Pipeline) {
        if pipeline.seq.len() > 1 {
            self.compound = true;
        }
        for command in &pipeline.seq {
            self.walk_command(command);
        }
    }

    fn walk_command(&mut self, command: &Command) {
        match command {
            Command::Simple(sc) => self.push_simple(sc),
            Command::Compound(compound, redirects) => {
                // A grouping/loop/conditional construct is inherently
                // compound; recurse into the commands it contains so each
                // is evaluated independently.
                self.compound = true;
                self.walk_compound_command(compound);
                if let Some(list) = redirects {
                    self.note_redirects(&list.0);
                }
            }
            // A function *definition* runs nothing on its own; its body
            // only executes when later called (and that call would be its
            // own simple command). Flag as compound so the chain can't be
            // remembered, but extract nothing to grant.
            Command::Function(_) => self.compound = true,
            // `[[ ... ]]` runs no external program — a shell builtin test.
            Command::ExtendedTest(_, _) => self.compound = true,
        }
    }

    fn walk_compound_command(&mut self, compound: &CompoundCommand) {
        match compound {
            CompoundCommand::Subshell(s) => self.walk_compound_list(&s.list),
            CompoundCommand::BraceGroup(b) => self.walk_compound_list(&b.list),
            CompoundCommand::ForClause(f) => {
                if let Some(body) = for_body(f) {
                    self.walk_compound_list(body);
                }
            }
            CompoundCommand::WhileClause(w) | CompoundCommand::UntilClause(w) => {
                self.walk_compound_list(&w.0);
                self.walk_compound_list(&w.1.list);
            }
            CompoundCommand::IfClause(i) => self.walk_if(i),
            CompoundCommand::CaseClause(c) => {
                for item in &c.cases {
                    if let Some(cmd) = &item.cmd {
                        self.walk_compound_list(cmd);
                    }
                }
            }
            // Arithmetic / arithmetic-for / coprocess run no statically
            // knowable external program from the loop scaffolding itself;
            // any embedded command list is handled where it appears. The
            // `compound` flag is already set by the caller, so these can't
            // be auto-granted regardless.
            CompoundCommand::Arithmetic(_)
            | CompoundCommand::ArithmeticForClause(_)
            | CompoundCommand::Coprocess(_) => {}
        }
    }

    fn walk_if(&mut self, clause: &ast::IfClauseCommand) {
        self.walk_compound_list(&clause.condition);
        self.walk_compound_list(&clause.then);
        if let Some(elses) = &clause.elses {
            for else_clause in elses {
                if let Some(cond) = &else_clause.condition {
                    self.walk_compound_list(cond);
                }
                self.walk_compound_list(&else_clause.body);
            }
        }
    }

    /// Extract argv[0], first subcommand, arguments and risk from a simple command and
    /// record it. Command/process substitution anywhere in the command
    /// marks the source compound (a substituted program isn't statically
    /// knowable) but the outer program is still keyed and evaluated.
    fn push_simple(&mut self, sc: &SimpleCommand) {
        // Prefix items are assignments / redirects only (the grammar
        // never puts the command name in the prefix); a redirect target
        // or assignment value can carry a substitution, so scan them.
        if let Some(prefix) = &sc.prefix {
            self.note_prefix_or_suffix(&prefix.0);
        }
        let Some(name_word) = &sc.word_or_name else {
            // No program word — a commandless line still executes: a
            // redirect (`>out`, `FOO=bar >out`) opens/truncates a file,
            // and a bare assignment (`FOO=bar`, `PATH=/evil/bin`,
            // `PROMPT_COMMAND=…`) mutates interactive-shell state. The
            // prefix scan above already recorded prefix redirects and
            // assignments; the grammar never produces a suffix without a
            // program word, but scanning it keeps that true by
            // construction instead of by assumption.
            if let Some(suffix) = &sc.suffix {
                self.note_prefix_or_suffix(&suffix.0);
            }
            return;
        };
        let program = name_word.value.clone();
        let executable = normalize_executable_word(&program);
        let normalized_program = executable
            .normalized
            .clone()
            .unwrap_or_else(|| program.clone());
        if word_has_substitution(&program) || !executable.persistable {
            self.compound = true;
        }

        // First subcommand token: the first suffix *word* that is a clean
        // bare identifier (`pr`, `push`, `build`) — not an option
        // (`-x`/`--flag`), not a quoted string, not a path operand
        // (`/tmp`, `./x`, `a/b`), not anything carrying shell
        // metacharacters. This keys `gh pr`, `git push`, `cargo build`
        // while leaving `cd /tmp`, `echo "a && b"`, `cat file.txt` keyed
        // on `argv[0]` alone: those first args are *operands*, not
        // subcommands, and a narrower key (no subcommand) is the safe
        // direction. `ls -la` and `./script` likewise have no subcommand.
        let mut args = Vec::new();
        let mut subcommand = None;
        let mut saw_first_non_option = false;
        if let Some(suffix) = &sc.suffix {
            self.note_prefix_or_suffix(&suffix.0);
            for item in &suffix.0 {
                match item {
                    CommandPrefixOrSuffixItem::Word(w) => {
                        args.push(w.value.clone());
                        // Stop at the first non-option word: if it's a clean
                        // subcommand token, take it; otherwise the command has
                        // no subcommand (its first operand is a value, not a
                        // verb). Either way we don't scan further — a later
                        // bare word is an argument to this operand.
                        if !saw_first_non_option && !w.value.starts_with('-') {
                            saw_first_non_option = true;
                            if is_subcommand_token(&w.value) {
                                subcommand = Some(w.value.clone());
                            }
                        }
                    }
                    CommandPrefixOrSuffixItem::AssignmentWord(_, w) => {
                        args.push(w.value.clone());
                    }
                    _ => {}
                }
            }
        }

        let wrapper = !executable.persistable
            || is_wrapper(&normalized_program)
            || is_privileged_non_persistable(&normalized_program);
        let risk = classify_risk(
            &normalized_program,
            subcommand.as_deref(),
            &args,
            wrapper,
            !executable.persistable,
        );
        let substitution_sources: Vec<String> = std::iter::once(program.as_str())
            .chain(args.iter().map(String::as_str))
            .flat_map(command_substitutions)
            .collect();

        let execution_bearing_option = has_execution_bearing_option(&args, &normalized_program);
        // Source span of this simple command within the original string,
        // from the AST's `SourceLocation`. `index` counts chars (the
        // tokenizer advances it once per `char`), so the range slices a
        // `char`-indexed view correctly. `end` is exclusive.
        let span = sc.location().map(|loc| CharSpan {
            start: loc.start.index,
            end: loc.end.index,
        });
        self.simple_commands.push(SimpleCommandInfo {
            program,
            normalized_program,
            subcommand,
            args,
            wrapper,
            execution_bearing_option,
            risk,
            span,
        });

        // Command substitutions execute their bodies before the outer command.
        // Parse every statically recoverable body so grants for the outer
        // command never authorize an embedded program.  An unparseable body
        // remains compound and therefore still fails closed by prompting.
        for source in substitution_sources {
            match classify(&source) {
                Classification::Parsed {
                    simple_commands, ..
                } => {
                    self.compound = true;
                    self.simple_commands.extend(simple_commands);
                }
                Classification::EffectsOnly
                | Classification::Empty
                | Classification::Unparseable(_) => {
                    self.compound = true;
                }
            }
        }
    }

    /// Scan prefix/suffix items: a redirect to a process-substitution, or
    /// a word/assignment carrying `$(...)`/backticks, means a dynamic
    /// program — mark compound (forces a prompt, never auto-granted).
    fn note_prefix_or_suffix(&mut self, items: &[CommandPrefixOrSuffixItem]) {
        for item in items {
            match item {
                CommandPrefixOrSuffixItem::Word(w) => {
                    if word_has_substitution(&w.value) {
                        self.compound = true;
                    }
                }
                CommandPrefixOrSuffixItem::AssignmentWord(_, w) => {
                    // Assignments are effects even without substitution:
                    // they mutate interactive-shell state. `PROMPT_COMMAND=…`
                    // runs at the next prompt; `PATH=/evil/bin` redirects
                    // later commands to attacker-controlled executables.
                    self.effects_only = true;
                    if word_has_substitution(&w.value) {
                        self.compound = true;
                    }
                }
                CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => {
                    self.compound = true;
                }
                CommandPrefixOrSuffixItem::IoRedirect(redir) => self.note_one_redirect(redir),
            }
        }
    }

    fn note_redirects(&mut self, redirects: &[IoRedirect]) {
        for redir in redirects {
            self.note_one_redirect(redir);
        }
    }

    fn note_one_redirect(&mut self, redir: &IoRedirect) {
        // Every I/O redirect is an effect (opens, truncates, or creates a
        // file; reads a descriptor) even on a line with no program word:
        // `>important-file` executes a filesystem mutation with no
        // program token at all. Recorded so "decomposed to no simple
        // command" can never be read as "no effect".
        self.effects_only = true;
        if let IoRedirect::File(_, _, IoFileRedirectTarget::ProcessSubstitution(_, _)) = redir {
            self.compound = true;
        }
    }
}

/// `for` loops: the do-group body. `body` is a `DoGroupCommand` whose
/// `list` holds the per-iteration commands.
fn for_body(f: &ast::ForClauseCommand) -> Option<&ast::CompoundList> {
    Some(&f.body.list)
}

/// Detect `$(...)` command substitution or backtick substitution inside
/// a word's raw text. The parser keeps these inline in the word value
/// (it doesn't expand them), so a textual scan is correct *and*
/// quote-aware: a `$(` inside a single-quoted segment is still a literal
/// the shell won't expand, but we conservatively flag any `$(`/backtick
/// since distinguishing single- from double-quote context here would
/// re-implement the tokenizer. Over-flagging only forces a prompt — the
/// safe direction.
fn word_has_substitution(word: &str) -> bool {
    word.contains("$(") || word.contains('`')
}

/// Extract balanced $(...) bodies for recursive classification. Backticks remain
/// compound and prompt fail-closed because decoding their shell quoting here would
/// duplicate the parser.
fn command_substitutions(word: &str) -> Vec<String> {
    let bytes = word.as_bytes();
    let mut bodies = Vec::new();
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] != b'$' || bytes[index + 1] != b'(' {
            index += 1;
            continue;
        }
        let start = index + 2;
        let mut depth = 1_u32;
        let mut cursor = start;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        bodies.push(word[start..cursor].to_owned());
                        index = cursor;
                        break;
                    }
                }
                _ => {}
            }
            cursor += 1;
        }
        if depth != 0 {
            break;
        }
        index += 1;
    }
    bodies
}

/// Whether a suffix word reads as a clean subcommand verb rather than an
/// operand. A subcommand is a short bare identifier — letters, digits,
/// `-`, `_` — with no path separator, no quotes, no shell
/// metacharacters, and not empty. `pr`/`push`/`build` qualify; `/tmp`,
/// `./x`, `a/b`, `file.txt`, and any quoted/substituted word do not. The
/// raw word value is what the parser kept (quotes included), so a quoted
/// arg fails the predicate naturally.
fn is_subcommand_token(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Values of these options can install a command string or executable path,
/// so permission decisions must review their invocations.
fn has_execution_bearing_option(args: &[String], program: &str) -> bool {
    let mut parse_options = true;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if !parse_options {
            break;
        }
        if arg == "--" {
            parse_options = false;
            index += 1;
            continue;
        }
        match program {
            "git" if arg == "-c" || arg.starts_with("-c") && arg.len() > 2 => return true,
            "git" if arg == "--config-env" || arg.starts_with("--config-env=") => return true,
            "rsync" if arg == "-e" || arg.starts_with("-e") && arg.len() > 2 => return true,
            "rsync" if arg == "--rsh" || arg.starts_with("--rsh=") => return true,
            "scp" if arg == "-S" || arg.starts_with("-S") && arg.len() > 2 => return true,
            "ssh" if arg == "-o" => {
                if let Some(value) = args.get(index + 1)
                    && (value.starts_with("ProxyCommand=") || value.starts_with("LocalCommand="))
                {
                    return true;
                }
            }
            "ssh"
                if let Some(value) = arg.strip_prefix("-o")
                    && (value.starts_with("ProxyCommand=")
                        || value.starts_with("LocalCommand=")) =>
            {
                return true;
            }
            _ => {}
        }
        index += 1;
    }
    false
}

/// Whether `program` (argv[0]) is a wrapper/eval command. Matches the
/// trailing path component too, so `/bin/bash` and `/usr/bin/sudo` are
/// caught, not just the bare names.
fn is_wrapper(program: &str) -> bool {
    let base = program_basename(program);
    WRAPPER_COMMANDS.contains(&base)
}

fn is_privileged_non_persistable(program: &str) -> bool {
    let base = program_basename(program);
    PRIVILEGED_NON_PERSISTABLE_COMMANDS.contains(&base)
}

fn program_basename(program: &str) -> &str {
    program.rsplit(['/', '\\']).next().unwrap_or(program)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExecutableWord {
    normalized: Option<String>,
    persistable: bool,
}

/// Reduce static shell quoting/escaping in argv[0]. Anything requiring shell
/// expansion is dynamic and must not become a persistent approval key.
fn normalize_executable_word(word: &str) -> ExecutableWord {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Normal,
        Single,
        Double,
    }

    let mut state = State::Normal;
    let mut out = String::new();
    let mut chars = word.chars().peekable();
    while let Some(ch) = chars.next() {
        match state {
            State::Normal => match ch {
                '\'' => state = State::Single,
                '"' => state = State::Double,
                '\\' => match chars.next() {
                    Some('\n') => {}
                    Some(next) => out.push(next),
                    None => {
                        return ExecutableWord {
                            normalized: None,
                            persistable: false,
                        };
                    }
                },
                '$' | '`' | '*' | '?' | '[' | ']' | '{' | '}' | '~' => {
                    return ExecutableWord {
                        normalized: None,
                        persistable: false,
                    };
                }
                _ => out.push(ch),
            },
            State::Single => match ch {
                '\'' => state = State::Normal,
                _ => out.push(ch),
            },
            State::Double => match ch {
                '"' => state = State::Normal,
                '$' | '`' => {
                    return ExecutableWord {
                        normalized: None,
                        persistable: false,
                    };
                }
                '\\' => match chars.next() {
                    Some('\n') => {}
                    Some(next @ ('$' | '`' | '"' | '\\')) => out.push(next),
                    Some(next) => {
                        out.push('\\');
                        out.push(next);
                    }
                    None => {
                        return ExecutableWord {
                            normalized: None,
                            persistable: false,
                        };
                    }
                },
                _ => out.push(ch),
            },
        }
    }

    let persistable =
        state == State::Normal && !out.is_empty() && !out.chars().any(char::is_whitespace);
    ExecutableWord {
        normalized: persistable.then_some(out),
        persistable,
    }
}

fn classify_risk(
    program: &str,
    subcommand: Option<&str>,
    args: &[String],
    wrapper: bool,
    dynamic_program: bool,
) -> RiskMetadata {
    let base = program_basename(program);
    let mut risk = RiskMetadata::default();

    if dynamic_program {
        risk.tier = RiskTier::Dynamic;
        risk.reasons.push("dynamic executable name".to_string());
    } else if is_privileged_non_persistable(program) {
        risk.tier = RiskTier::Privileged;
        match base {
            "busybox" => risk
                .reasons
                .push("privileged applet dispatcher".to_string()),
            _ => risk
                .reasons
                .push("privileged execution context".to_string()),
        }
    } else if wrapper {
        risk.tier = match base {
            "sudo" | "doas" => RiskTier::Privileged,
            _ => RiskTier::Dynamic,
        };
        match base {
            "sudo" | "doas" => risk.reasons.push("privilege escalation".to_string()),
            "ssh" => risk.reasons.push("remote execution".to_string()),
            "find" if args.iter().any(|arg| arg == "-exec" || arg == "-execdir") => {
                risk.reasons.push("find -exec dynamic command".to_string());
            }
            "find" => risk
                .reasons
                .push("dynamic file traversal command".to_string()),
            "bash" | "sh" | "zsh" if shell_reads_script_from_stdin(args) => risk
                .reasons
                .push("shell reads script from stdin".to_string()),
            _ => risk.reasons.push("dynamic command string".to_string()),
        }
    }

    match base {
        "rm" => {
            risk.tier = max_tier(risk.tier, RiskTier::Destructive);
            risk.reasons.push("removes files".to_string());
            if args.iter().any(|arg| rm_recursive_arg(arg)) {
                risk.reasons.push("recursive".to_string());
            }
            if args.iter().any(|arg| rm_force_arg(arg)) {
                risk.reasons.push("force".to_string());
            }
            collect_literal_paths(args, OperandShape::AllNonOptions, &mut risk);
        }
        "mv" => {
            risk.tier = max_tier(risk.tier, RiskTier::Mutating);
            risk.reasons.push("moves or renames paths".to_string());
            collect_literal_paths(args, OperandShape::AllNonOptions, &mut risk);
        }
        "cp" => {
            risk.tier = max_tier(risk.tier, RiskTier::Mutating);
            risk.reasons.push("copies files".to_string());
            if args.iter().any(|arg| cp_recursive_arg(arg)) {
                risk.reasons.push("recursive".to_string());
            }
            collect_literal_paths(args, OperandShape::AllNonOptions, &mut risk);
        }
        "mkdir" => {
            risk.tier = max_tier(risk.tier, RiskTier::Mutating);
            risk.reasons.push("creates directories".to_string());
            collect_literal_paths(args, OperandShape::Mkdir, &mut risk);
        }
        "chmod" => {
            risk.tier = max_tier(risk.tier, RiskTier::Mutating);
            risk.reasons.push("changes file permissions".to_string());
            if args.iter().any(|arg| arg == "777" || arg.ends_with("=rwx")) {
                risk.reasons.push("broad permissions".to_string());
            }
            collect_literal_paths(args, OperandShape::ModeThenPaths, &mut risk);
        }
        "chown" => {
            risk.tier = max_tier(risk.tier, RiskTier::Privileged);
            risk.reasons.push("changes file ownership".to_string());
            collect_literal_paths(args, OperandShape::ModeThenPaths, &mut risk);
        }
        "cat" if args.iter().any(|arg| !arg.starts_with('-')) => {
            risk.native_tool_hints
                .push("Use `read` for precise file reads.".to_string());
        }
        "grep" | "rg" => {
            risk.native_tool_hints.push(
                "Use `grep`/`search` tools for scoped project search when possible.".to_string(),
            );
        }
        _ => {}
    }

    apply_builtin_dangerous_flag_rules(base, subcommand, args, &mut risk);

    dedup(&mut risk.reasons);
    dedup(&mut risk.affected_paths);
    dedup(&mut risk.native_tool_hints);
    risk
}

#[derive(Clone, Copy)]
struct BuiltinDangerousFlagRule {
    program: &'static str,
    subcommand: Option<&'static str>,
    tier: RiskTier,
    reason: &'static str,
    matches: fn(&[String]) -> bool,
}

const BUILTIN_DANGEROUS_FLAG_RULES: &[BuiltinDangerousFlagRule] = &[
    BuiltinDangerousFlagRule {
        program: "git",
        subcommand: Some("push"),
        tier: RiskTier::Destructive,
        reason: "dangerous git push flag",
        matches: git_push_dangerous_flag,
    },
    BuiltinDangerousFlagRule {
        program: "git",
        subcommand: Some("reset"),
        tier: RiskTier::Destructive,
        reason: "hard reset",
        matches: git_reset_hard_flag,
    },
    BuiltinDangerousFlagRule {
        program: "git",
        subcommand: Some("clean"),
        tier: RiskTier::Destructive,
        reason: "force clean",
        matches: git_clean_force_flag,
    },
    BuiltinDangerousFlagRule {
        program: "rm",
        subcommand: None,
        tier: RiskTier::Destructive,
        reason: "recursive or force removal",
        matches: rm_recursive_or_force_flag,
    },
    BuiltinDangerousFlagRule {
        program: "chmod",
        subcommand: None,
        tier: RiskTier::Destructive,
        reason: "broad permissions",
        matches: chmod_broad_permissions_flag,
    },
    BuiltinDangerousFlagRule {
        program: "dd",
        subcommand: None,
        tier: RiskTier::Destructive,
        reason: "writes output operand",
        matches: dd_output_operand,
    },
    BuiltinDangerousFlagRule {
        program: "kubectl",
        subcommand: Some("delete"),
        tier: RiskTier::Destructive,
        reason: "kubectl delete",
        matches: always_matches,
    },
    BuiltinDangerousFlagRule {
        program: "docker",
        subcommand: Some("rm"),
        tier: RiskTier::Destructive,
        reason: "force container or image removal",
        matches: container_force_removal_flag,
    },
    BuiltinDangerousFlagRule {
        program: "docker",
        subcommand: Some("rmi"),
        tier: RiskTier::Destructive,
        reason: "force container or image removal",
        matches: container_force_removal_flag,
    },
    BuiltinDangerousFlagRule {
        program: "docker",
        subcommand: Some("system"),
        tier: RiskTier::Destructive,
        reason: "system prune",
        matches: container_system_prune,
    },
    BuiltinDangerousFlagRule {
        program: "podman",
        subcommand: Some("rm"),
        tier: RiskTier::Destructive,
        reason: "force container or image removal",
        matches: container_force_removal_flag,
    },
    BuiltinDangerousFlagRule {
        program: "podman",
        subcommand: Some("rmi"),
        tier: RiskTier::Destructive,
        reason: "force container or image removal",
        matches: container_force_removal_flag,
    },
    BuiltinDangerousFlagRule {
        program: "podman",
        subcommand: Some("system"),
        tier: RiskTier::Destructive,
        reason: "system prune",
        matches: container_system_prune,
    },
    BuiltinDangerousFlagRule {
        program: "npm",
        subcommand: Some("publish"),
        tier: RiskTier::Destructive,
        reason: "package publish",
        matches: always_matches,
    },
    BuiltinDangerousFlagRule {
        program: "pnpm",
        subcommand: Some("publish"),
        tier: RiskTier::Destructive,
        reason: "package publish",
        matches: always_matches,
    },
    BuiltinDangerousFlagRule {
        program: "yarn",
        subcommand: Some("publish"),
        tier: RiskTier::Destructive,
        reason: "package publish",
        matches: always_matches,
    },
];

fn apply_builtin_dangerous_flag_rules(
    program: &str,
    subcommand: Option<&str>,
    args: &[String],
    risk: &mut RiskMetadata,
) {
    for rule in BUILTIN_DANGEROUS_FLAG_RULES {
        if rule.program == program
            && rule
                .subcommand
                .is_none_or(|rule_subcommand| Some(rule_subcommand) == subcommand)
            && (rule.matches)(args)
        {
            risk.tier = max_tier(risk.tier, rule.tier);
            risk.reasons.push(rule.reason.to_string());
        }
    }
}

fn has_any_arg(args: &[String], flags: &[&str]) -> bool {
    args.iter().any(|arg| flags.contains(&arg.as_str()))
}

fn git_push_dangerous_flag(args: &[String]) -> bool {
    has_any_arg(
        args,
        &["--force", "-f", "--force-with-lease", "--delete", "-d"],
    ) || args
        .iter()
        .any(|arg| arg.starts_with("--force-with-lease="))
}

fn git_reset_hard_flag(args: &[String]) -> bool {
    has_any_arg(args, &["--hard"])
}

fn git_clean_force_flag(args: &[String]) -> bool {
    has_any_arg(args, &["-f", "-fd", "-fdx", "--force"])
}

fn rm_recursive_or_force_flag(args: &[String]) -> bool {
    args.iter()
        .any(|arg| rm_recursive_arg(arg) || rm_force_arg(arg))
}

fn chmod_broad_permissions_flag(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "777" || arg.ends_with("=rwx"))
}

fn dd_output_operand(args: &[String]) -> bool {
    args.iter().any(|arg| arg.starts_with("of="))
}

fn always_matches(_: &[String]) -> bool {
    true
}

fn container_force_removal_flag(args: &[String]) -> bool {
    has_any_arg(args, &["-f", "--force"])
}

fn container_system_prune(args: &[String]) -> bool {
    args.get(1).is_some_and(|arg| arg == "prune")
}

fn shell_reads_script_from_stdin(args: &[String]) -> bool {
    // Pipelines do not preserve edge metadata in `SimpleCommandInfo`, so a
    // bare shell stage is classified by its own argv shape: no script operand
    // means the shell may read its program from stdin.
    args.is_empty()
}

#[derive(Debug, Clone, Copy)]
enum OperandShape {
    AllNonOptions,
    ModeThenPaths,
    Mkdir,
}

fn collect_literal_paths(args: &[String], shape: OperandShape, risk: &mut RiskMetadata) {
    let mut skip_next = false;
    let mut skipped_mode = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            if matches!(shape, OperandShape::Mkdir) && (arg == "-m" || arg == "--mode") {
                skip_next = true;
            }
            continue;
        }
        if matches!(shape, OperandShape::ModeThenPaths) && !skipped_mode {
            skipped_mode = true;
            continue;
        }
        if dynamic_path_operand(arg) {
            risk.tier = max_tier(risk.tier, RiskTier::Dynamic);
            risk.reasons
                .push("dynamic or globbed path operand".to_string());
            continue;
        }
        risk.affected_paths.push(arg.clone());
    }
}

fn dynamic_path_operand(arg: &str) -> bool {
    arg.contains('$')
        || arg.contains('`')
        || arg.contains('*')
        || arg.contains('?')
        || arg.contains('[')
        || arg.contains(']')
        || arg.starts_with('~')
        || arg.contains('{')
        || arg.contains('}')
}

fn rm_recursive_arg(arg: &str) -> bool {
    arg == "-r"
        || arg == "-R"
        || arg == "--recursive"
        || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('r'))
}

fn rm_force_arg(arg: &str) -> bool {
    arg == "-f"
        || arg == "--force"
        || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('f'))
}

fn cp_recursive_arg(arg: &str) -> bool {
    arg == "-r"
        || arg == "-R"
        || arg == "-a"
        || arg == "--recursive"
        || arg == "--archive"
        || (arg.starts_with('-')
            && !arg.starts_with("--")
            && (arg.contains('r') || arg.contains('R')))
}

fn max_tier(a: RiskTier, b: RiskTier) -> RiskTier {
    a.max(b)
}

fn dedup(values: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}
