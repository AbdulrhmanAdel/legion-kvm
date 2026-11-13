use super::*;
#[test]
fn recipe_checks_bounds_size_and_result_digest() {
    let mut r = Recipe {
        schema: 1,
        source_sha256: String::new(),
        unix_sha256: String::new(),
        target_sha256: format!("{:x}", Sha256::digest(b"old-new")),
        target_bytes: 7,
        pieces: vec![
            Piece::Copy {
                source: 0,
                length: 3,
            },
            Piece::Literal {
                data: "2d6e6577".into(),
            },
        ],
    };
    assert_eq!(apply(&r, b"old").expect("valid recipe"), b"old-new");
    assert!(apply(&r, b"x").is_err());
    r.target_bytes = 6;
    assert!(apply(&r, b"old").is_err());
    r.target_bytes = 7;
    r.target_sha256 = "bad".into();
    assert!(apply(&r, b"old").is_err());
}
