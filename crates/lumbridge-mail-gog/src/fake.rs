//! A scripted [`Runner`] for tests: answers calls from recorded (made-up) gog output and keeps a log of
//! every call so tests can check the exact command lines, guard flags included.

use crate::runner::{GogCall, GogOutput, Runner};
use lumbridge_mail_core::{Error, Result};
use std::sync::Mutex;

type Reply = std::result::Result<GogOutput, Error>;

struct Rule {
    command: String,
    contains: Vec<String>,
    reply: Reply,
    /// None = answer every time; Some(n) = answer n more times, then stop matching.
    uses: Option<usize>,
}

/// Rules are tried newest first; the first whose command matches and whose words all appear in the
/// call's literal arguments answers. A call no rule answers fails the test with a clear message
/// (returned as `Error::Other("fake: no rule for ...")`).
#[derive(Default)]
pub struct FakeRunner {
    rules: Mutex<Vec<Rule>>,
    calls: Mutex<Vec<GogCall>>,
}

impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer `command` (dotted, e.g. `gmail.raw`) calls whose arguments contain all of `contains`.
    pub fn on(&self, command: &str, contains: &[&str], reply: GogOutput) -> &Self {
        self.push(command, contains, Ok(reply), None)
    }

    /// Like `on`, but only for the next `n` matching calls.
    pub fn on_times(&self, n: usize, command: &str, contains: &[&str], reply: GogOutput) -> &Self {
        self.push(command, contains, Ok(reply), Some(n))
    }

    /// Answer with a runner error (a timeout, ssh down) instead of gog output.
    pub fn on_err(&self, command: &str, contains: &[&str], err: Error) -> &Self {
        self.push(command, contains, Err(err), None)
    }

    pub fn on_json(&self, command: &str, contains: &[&str], v: serde_json::Value) -> &Self {
        self.on(command, contains, GogOutput::ok_json(v))
    }

    fn push(&self, command: &str, contains: &[&str], reply: Reply, uses: Option<usize>) -> &Self {
        self.rules.lock().unwrap().push(Rule {
            command: command.into(),
            contains: contains.iter().map(|s| s.to_string()).collect(),
            reply,
            uses,
        });
        self
    }

    pub fn calls(&self) -> Vec<GogCall> {
        self.calls.lock().unwrap().clone()
    }

    /// The dotted commands run so far, in order.
    pub fn commands(&self) -> Vec<String> {
        self.calls().iter().map(|c| c.command().unwrap_or("?").to_owned()).collect()
    }

    pub fn calls_to(&self, command: &str) -> Vec<GogCall> {
        self.calls().into_iter().filter(|c| c.command() == Some(command)).collect()
    }

    pub fn clear_calls(&self) {
        self.calls.lock().unwrap().clear();
    }
}

#[async_trait::async_trait]
impl Runner for FakeRunner {
    async fn run(&self, call: GogCall) -> Result<GogOutput> {
        self.calls.lock().unwrap().push(call.clone());
        let args = call.display_args();
        let cmd = call.command().unwrap_or("").to_owned();
        let mut rules = self.rules.lock().unwrap();
        for rule in rules.iter_mut().rev() {
            if rule.command != cmd || !rule.contains.iter().all(|w| args.iter().any(|a| a == w)) {
                continue;
            }
            match &mut rule.uses {
                Some(0) => continue,
                Some(n) => *n -= 1,
                None => {}
            }
            return rule.reply.clone();
        }
        Err(Error::Other(format!("fake: no rule for {cmd} {args:?}")))
    }
}
