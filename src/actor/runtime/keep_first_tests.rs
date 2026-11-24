use super::*;

/// The first panic is kept; each later one is discarded and, with the
/// `tracing` feature, logged, naming the actor.
#[test]
fn a_discarded_panic_is_logged_naming_the_actor() {
    #[cfg(feature = "tracing")]
    let logged = crate::actor::supervision::lost_runner::capture::install();
    let token = uuid::Uuid::new_v4();
    let owner = Owner::new(ActorId::from(format!("keep-first-{token}")), false);
    let mut first = None;
    keep_first_of(&mut first, None, &owner);
    keep_first_of(&mut first, Some(format!("kept {token}")), &owner);
    keep_first_of(&mut first, Some(format!("second {token}")), &owner);
    keep_first_of(&mut first, Some(format!("third {token}")), &owner);
    keep_first_of(&mut first, None, &owner);
    assert_eq!(first, Some(format!("kept {token}")));
    #[cfg(feature = "tracing")]
    {
        let lines: Vec<String> = logged
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains(&token.to_string()))
            .cloned()
            .collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        for which in ["second", "third"] {
            assert!(
                lines.iter().any(
                    |line| line.contains(which) && line.contains(&format!("actor_id={owner} "))
                ),
                "{which}: {lines:?}"
            );
        }
        assert!(!lines
            .iter()
            .any(|line| line.contains(&format!("kept {token}"))));
    }
}
