//! A scripted [`Model`] for tests (this crate's, and the daemon's and app's): answers come from a
//! queue or a function of the request, and every request is recorded so tests can check what a model
//! was shown.

use crate::model::{ChatRequest, Model};
use lumbridge_mail_core::safety::ModelPlace;
use lumbridge_mail_core::{Error, Result};
use std::collections::VecDeque;
use std::sync::Mutex;

type Responder = Box<dyn Fn(&ChatRequest) -> Result<String> + Send + Sync>;

pub struct ScriptedModel {
    place: ModelPlace,
    label: String,
    queue: Mutex<VecDeque<Result<String>>>,
    responder: Option<Responder>,
    seen: Mutex<Vec<ChatRequest>>,
}

impl ScriptedModel {
    /// Answers from `answers`, in order; when they run out, `Error::Other("script ran out")`.
    pub fn new(answers: impl IntoIterator<Item = Result<String>>) -> Self {
        Self {
            place: ModelPlace::Local,
            label: "fake".into(),
            queue: Mutex::new(answers.into_iter().collect()),
            responder: None,
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Plain-text answers, in order.
    pub fn answers<S: Into<String>>(answers: impl IntoIterator<Item = S>) -> Self {
        Self::new(answers.into_iter().map(|s| Ok(s.into())))
    }

    /// Answers computed from each request (after the queue is used up).
    pub fn with(f: impl Fn(&ChatRequest) -> Result<String> + Send + Sync + 'static) -> Self {
        Self { responder: Some(Box::new(f)), ..Self::new([]) }
    }

    pub fn at(mut self, place: ModelPlace) -> Self {
        self.place = place;
        self
    }

    pub fn labelled(mut self, label: &str) -> Self {
        self.label = label.into();
        self
    }

    /// Every request so far, oldest first.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.seen.lock().expect("lock").clone()
    }

    pub fn calls(&self) -> usize {
        self.seen.lock().expect("lock").len()
    }
}

#[async_trait::async_trait]
impl Model for ScriptedModel {
    fn place(&self) -> ModelPlace {
        self.place
    }
    fn label(&self) -> &str {
        &self.label
    }
    async fn chat(&self, req: &ChatRequest) -> Result<String> {
        self.seen.lock().expect("lock").push(req.clone());
        if let Some(a) = self.queue.lock().expect("lock").pop_front() {
            return a;
        }
        match &self.responder {
            Some(f) => f(req),
            None => Err(Error::Other("script ran out".into())),
        }
    }
}
