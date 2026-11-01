//! Fenced-code highlighting with syntect. Syntect parses the grammar; scopes
//! map onto the TUI palette (so 256-color terminals and light/dark themes keep
//! working) instead of a syntect color theme. Unknown languages render plain.
use crate::theme::theme;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use std::sync::OnceLock;
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn find(name: &str) -> Option<&'static SyntaxReference> {
    let set = syntaxes();
    let token = match name.to_ascii_lowercase().as_str() {
        "" => return None,
        "ts" | "typescript" | "tsx" | "jsx" => "js".to_string(),
        "sh" | "shell" | "zsh" | "console" => "bash".to_string(),
        "jsonc" => "json".to_string(),
        "toml" | "ini" => "ini".to_string(),
        other => other.to_string(),
    };
    set.find_syntax_by_token(&token)
        .or_else(|| set.find_syntax_by_extension(&token))
}

/// Highlighter for one fenced block; parse state carries across its lines.
pub struct Highlighter {
    state: Option<(ParseState, ScopeStack)>,
}

impl Highlighter {
    pub fn new(name: &str) -> Self {
        let lang = name.split([' ', ',', '{']).next().unwrap_or_default();
        Self {
            state: find(lang).map(|s| (ParseState::new(s), ScopeStack::new())),
        }
    }

    /// Whether a language (fence name or file extension) has a grammar.
    pub fn knows(name: &str) -> bool {
        find(name).is_some()
    }

    pub fn line(&mut self, text: &str, bg: Style) -> Vec<Span<'static>> {
        let t = theme();
        let plain = bg.fg(t.code_fg);
        let Some((parse, stack)) = &mut self.state else {
            return vec![Span::styled(text.to_string(), plain)];
        };
        let line = format!("{text}\n");
        let Ok(ops) = parse.parse_line(&line, syntaxes()) else {
            self.state = None;
            return vec![Span::styled(text.to_string(), plain)];
        };
        let mut out = Vec::new();
        let mut pos = 0;
        let mut emit = |out: &mut Vec<Span<'static>>, end: usize, stack: &ScopeStack| {
            let end = end.min(text.len());
            if end > pos {
                let color = color_for(stack).unwrap_or(t.code_fg);
                out.push(Span::styled(text[pos..end].to_string(), bg.fg(color)));
                pos = end;
            }
        };
        for (at, op) in ops {
            emit(&mut out, at, stack);
            let _ = stack.apply(&op);
        }
        emit(&mut out, text.len(), stack);
        out
    }
}

/// Innermost scope wins.
fn color_for(stack: &ScopeStack) -> Option<Color> {
    let t = theme();
    for scope in stack.as_slice().iter().rev() {
        let s = scope.build_string();
        let has = |p: &str| s.starts_with(p);
        let c = if has("comment") || has("punctuation.definition.comment") {
            t.comment
        } else if has("string") || has("constant.character") || has("markup.inserted") {
            if has("markup.inserted") {
                t.green
            } else {
                t.string
            }
        } else if has("markup.deleted") {
            t.red
        } else if has("constant.numeric") || has("constant.language") {
            t.number
        } else if has("keyword") || has("storage") {
            t.keyword
        } else if has("entity.name.function") || has("support.function") {
            t.heading
        } else if has("entity.name.type")
            || has("entity.name.class")
            || has("support.type")
            || has("support.class")
            || has("markup.heading")
            || has("meta.diff.range")
        {
            t.link
        } else if has("variable.parameter") || has("entity.name.tag") {
            t.accent
        } else {
            continue;
        };
        return Some(c);
    }
    None
}
