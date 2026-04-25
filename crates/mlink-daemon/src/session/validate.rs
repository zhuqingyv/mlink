pub(super) fn valid_room_code(code: &str) -> bool {
    code.len() == 6 && code.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_room_code_accepts_six_digits() {
        assert!(valid_room_code("123456"));
        assert!(valid_room_code("000000"));
    }

    #[test]
    fn valid_room_code_rejects_length_and_non_digit() {
        assert!(!valid_room_code(""));
        assert!(!valid_room_code("12345"));
        assert!(!valid_room_code("1234567"));
        assert!(!valid_room_code("12345a"));
    }
}
