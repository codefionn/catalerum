use super::*;

/// A recognized composer slash command (SOUL §12/§23).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SlashCmd {
    /// `/new` — start a fresh chat.
    New,
    /// `/<skill>` — invoke the named skill (the canonical `Skill::name`).
    Skill(String),
}

/// Parse a draft that names a slash command: a leading `/` followed by `new` or
/// a skill name, then optional arguments. Returns the command plus its trimmed
/// arguments, or `None` to send the draft as typed (a message may legitimately
/// start with a path like `/etc/hosts`).
///
/// `new` is checked first, so it shadows a same-named skill. Skill names may
/// contain spaces, so matching tries the longest exact name prefix first (byte
/// slicing on the raw draft stays safe that way), then falls back to a
/// case-insensitive match on the first whitespace-delimited token.
pub(super) fn parse_slash_command(draft: &str, skills: &[Skill]) -> Option<(SlashCmd, String)> {
    let rest = draft.trim().strip_prefix('/')?;
    let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (token, tail) = rest.split_at(token_end);
    if token.eq_ignore_ascii_case("new") {
        return Some((SlashCmd::New, tail.trim().to_string()));
    }
    let exact = skills
        .iter()
        .filter(|s| {
            rest == s.name
                || rest
                    .strip_prefix(&s.name)
                    .is_some_and(|r| r.starts_with(char::is_whitespace))
        })
        .max_by_key(|s| s.name.len());
    let skill = exact.or_else(|| skills.iter().find(|s| s.name.eq_ignore_ascii_case(token)))?;
    let args = rest
        .strip_prefix(&skill.name)
        .unwrap_or(tail)
        .trim()
        .to_string();
    Some((SlashCmd::Skill(skill.name.clone()), args))
}
