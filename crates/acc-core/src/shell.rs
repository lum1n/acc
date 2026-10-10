//! POSIX shell quoting for commands run on hosts over SSH.
//!
//! Every remote command ACC builds goes through [`quote`], so paths, prompts
//! and session ids can never break out of their argument.

/// Quotes one argument for a POSIX `sh`.
pub fn quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b))
    {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Joins a program and its arguments into one quoted command line.
pub fn join<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts.into_iter().map(quote).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_safe_words_alone() {
        assert_eq!(quote("--resume"), "--resume");
        assert_eq!(quote("/home/v/.acc/s1"), "/home/v/.acc/s1");
    }

    #[test]
    fn quotes_spaces_quotes_and_empty() {
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
    }
}
