//! Test runner (M6, track T): the test tree of a run (suites, tests, outcomes, durations),
//! rerun failed, test history and coverage reports.

use crate::Session;

#[derive(Default)]
pub struct TestingState {}

pub(crate) fn pump(_s: &mut Session) -> bool {
    false
}
