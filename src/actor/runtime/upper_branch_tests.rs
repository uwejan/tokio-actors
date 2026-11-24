use super::{SupervisionEvent, UpperBranch};

impl UpperBranch {
    fn doc_name(self) -> &'static str {
        match self {
            UpperBranch::Stop => "stop lane",
            UpperBranch::System => "system channel",
            UpperBranch::Restart => "restart plane",
            UpperBranch::Death => "death plane",
            UpperBranch::Verdict => "verdict signal",
            UpperBranch::Forwarder => "forwarder plane",
            UpperBranch::Priority => "priority lane",
        }
    }
}

#[test]
fn the_fast_path_clauses_follow_the_documented_reception_order() {
    let src = include_str!("../runtime.rs");
    let head = "Reception order, highest first:";
    let start = src
        .find(head)
        .expect("the module docs state the reception order")
        + head.len();
    let end = start
        + src[start..]
            .find(';')
            .expect("the order ends with a semicolon");
    let listed: Vec<String> = src[start..end]
        .replace("//!", " ")
        .split(',')
        .map(|name| name.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    let mut expected: Vec<String> = UpperBranch::ORDER
        .iter()
        .map(|b| b.doc_name().to_string())
        .collect();
    expected.push("mailbox".to_string());
    assert_eq!(listed, expected);
    assert!(UpperBranch::ORDER.contains(&SupervisionEvent::Verdict.branch()));
}
