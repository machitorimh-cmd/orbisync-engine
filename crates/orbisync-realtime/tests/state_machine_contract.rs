//! Contract test for the realtime connection state machine.
//!
//! `contracts/realtime-connection-state-machine.json` is the source of truth.
//! The comparison runs in both directions, so neither the contract nor the Rust
//! table can drift without failing CI.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use orbisync_realtime::{ConnectionState, TRANSITIONS};

fn contract() -> serde_json::Value {
    let path =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"))
            .join("../../contracts/realtime-connection-state-machine.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    serde_json::from_str(&raw).expect("contract must be valid JSON")
}

fn strings(value: &serde_json::Value) -> BTreeSet<String> {
    value
        .as_array()
        .expect("expected a JSON array")
        .iter()
        .map(|item| item.as_str().expect("expected a JSON string").to_owned())
        .collect()
}

#[test]
fn test_states_match_the_contract() {
    let contract = contract();
    let implemented: BTreeSet<String> = TRANSITIONS
        .iter()
        .flat_map(|(from, _, to)| [from.as_str().to_owned(), to.as_str().to_owned()])
        .collect();
    let declared = strings(&contract["states"]);
    assert_eq!(
        declared, implemented,
        "every contract state must appear in the implemented transition table"
    );
}

#[test]
fn test_events_match_the_contract() {
    let contract = contract();
    let implemented: BTreeSet<String> = TRANSITIONS
        .iter()
        .map(|(_, event, _)| event.as_str().to_owned())
        .collect();
    assert_eq!(strings(&contract["events"]), implemented);
}

#[test]
fn test_transitions_match_the_contract_in_both_directions() {
    let contract = contract();
    let declared: BTreeSet<(String, String, String)> = contract["transitions"]
        .as_array()
        .expect("transitions array")
        .iter()
        .map(|entry| {
            (
                entry["from"].as_str().expect("from").to_owned(),
                entry["event"].as_str().expect("event").to_owned(),
                entry["to"].as_str().expect("to").to_owned(),
            )
        })
        .collect();
    let implemented: BTreeSet<(String, String, String)> = TRANSITIONS
        .iter()
        .map(|(from, event, to)| {
            (
                from.as_str().to_owned(),
                event.as_str().to_owned(),
                to.as_str().to_owned(),
            )
        })
        .collect();

    assert_eq!(declared, implemented);
    assert_eq!(
        TRANSITIONS.len(),
        declared.len(),
        "no duplicate transitions"
    );
}

#[test]
fn test_initial_and_terminal_states_match_the_contract() {
    let contract = contract();
    assert_eq!(
        contract["initial"].as_str().expect("initial"),
        ConnectionState::INITIAL.as_str()
    );

    let declared_terminal = strings(&contract["terminal"]);
    let implemented_terminal: BTreeSet<String> = TRANSITIONS
        .iter()
        .flat_map(|(from, _, to)| [*from, *to])
        .filter(|state| state.is_terminal())
        .map(|state| state.as_str().to_owned())
        .collect();
    assert_eq!(declared_terminal, implemented_terminal);
}
