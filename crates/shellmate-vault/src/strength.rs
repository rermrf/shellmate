//! A deliberately modest heuristic, not an entropy estimate.
pub const PASSWORD_MINIMUM: &str =
    "口令强度至少需要达到「中」：建议 12 位以上并混合大小写、数字或符号";
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Strength {
    Weak,
    Medium,
    High,
}
impl Strength {
    pub fn label(self) -> &'static str {
        match self {
            Self::Weak => "弱",
            Self::Medium => "中",
            Self::High => "高",
        }
    }
}
pub fn strength(password: &str) -> Strength {
    let n = password.chars().count();
    let lower = zeroize::Zeroizing::new(password.to_lowercase());
    let classes = [
        password.chars().any(char::is_lowercase),
        password.chars().any(char::is_uppercase),
        password.chars().any(|c| c.is_ascii_digit()),
        password.chars().any(|c| !c.is_alphanumeric()),
    ]
    .into_iter()
    .filter(|v| *v)
    .count();
    let unique = password
        .char_indices()
        .filter(|(i, c)| !password[..*i].contains(*c))
        .count();
    if n < 8
        || unique < 4
        || [
            "password", "123456", "qwerty", "letmein", "admin", "abcdef", "密码",
        ]
        .iter()
        .any(|p| lower.contains(p))
    {
        return Strength::Weak;
    }
    if n >= 16 && unique >= 8 || n >= 12 && classes >= 3 {
        Strength::High
    } else if n >= 10 && classes >= 2 || n >= 8 && classes >= 3 || n >= 14 {
        Strength::Medium
    } else {
        Strength::Weak
    }
}
pub fn acceptable(password: &str) -> bool {
    strength(password) >= Strength::Medium
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn honest_table() {
        for (p, s, ok) in [
            ("", Strength::Weak, false),
            ("1234567890", Strength::Weak, false),
            ("Password123!", Strength::Weak, false),
            ("aaaaaaaaaaaaaaaa", Strength::Weak, false),
            ("zR7!kT2?", Strength::Medium, true),
            ("abcxyz", Strength::Weak, false),
            ("long meadow river stone", Strength::High, true),
            ("Zebra9!Rocks?", Strength::High, true),
        ] {
            assert_eq!(strength(p), s, "{p}");
            assert_eq!(acceptable(p), ok, "{p}")
        }
    }
}
