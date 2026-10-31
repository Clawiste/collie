pub fn add(a: i32, b: i32) -> i32 {
    let unused = 1;
    a + b
}

#[cfg(test)]
mod tests {
    #[test]
    fn passes() {
        assert_eq!(super::add(1, 2), 3);
    }

    #[test]
    fn fails() {
        assert_eq!(super::add(1, 2), 4);
    }

    #[test]
    #[ignore]
    fn ignored() {}
}
