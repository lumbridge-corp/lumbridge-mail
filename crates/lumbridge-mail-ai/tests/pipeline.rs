//! The AI layer end to end on the fake model: guards, rules before models, validation and repair,
//! batching, drafts, ask and extraction.

use lumbridge_mail_ai::testkit::*;
use lumbridge_mail_ai::*;
use lumbridge_mail_core::safety::ModelPlace;
use lumbridge_mail_core::*;
use std::sync::Arc;

const NOW: i64 = 1_794_334_000; // 2026-11-10

fn person() -> MailThread {
    thread1(msg("ada@example.com", Some("Ada"), "Lunch on Friday?", "Are you free Friday at noon? Let me know by Thursday.", &[]))
}

fn ai_with(model: ScriptedModel) -> (Arc<ScriptedModel>, MailAi) {
    let m = Arc::new(model);
    (m.clone(), MailAi::from_arc(m, AiConfig::default()).with_clock(|| NOW))
}

const GOOD: &str = r#"{"category":"personal","priority":55,"summary":"Ada asks if you are free for lunch on Friday","needs_reply":true,"due":"2026-11-13"}"#;

#[tokio::test]
async fn the_guard_refuses_what_policy_forbids() {
    let (m, ai) = ai_with(ScriptedModel::answers([GOOD]).at(ModelPlace::Hosted));
    assert!(matches!(ai.for_account(&account(Persona::Anon, AiPolicy::Any)), Err(Error::Refused(_))));
    assert!(matches!(ai.for_account(&account(Persona::Work, AiPolicy::LocalOnly)), Err(Error::Refused(_))));
    assert!(ai.for_account(&account(Persona::Work, AiPolicy::Any)).is_ok());
    let (_, local) = ai_with(ScriptedModel::answers([GOOD]));
    assert!(matches!(local.for_account(&account(Persona::Personal, AiPolicy::Off)), Err(Error::Refused(_))));
    assert_eq!(m.calls(), 0);
}

#[tokio::test]
async fn a_thread_from_another_account_is_refused() {
    let (m, ai) = ai_with(ScriptedModel::answers([GOOD]));
    let mut acct = account(Persona::Personal, AiPolicy::LocalOnly);
    acct.id = AccountId::new("other");
    let a = ai.for_account(&acct).unwrap();
    assert!(matches!(a.triage(&person()).await, Err(Error::Refused(_))));
    assert!(matches!(a.summarise(&person()).await, Err(Error::Refused(_))));
    assert_eq!(m.calls(), 0);
}

#[tokio::test]
async fn ask_refuses_when_no_account_is_allowed() {
    struct None_;
    #[async_trait::async_trait]
    impl Search for None_ {
        async fn search(&self, _: &str, _: &[AccountId], _: u32) -> Result<Vec<SearchHit>> {
            Ok(vec![])
        }
    }
    let (_, ai) = ai_with(ScriptedModel::answers([GOOD]).at(ModelPlace::Hosted));
    let r = ai.ask("q", &[account(Persona::Anon, AiPolicy::Any)], &None_).await;
    assert!(matches!(r, Err(Error::Refused(_))));
}

#[tokio::test]
async fn triage_by_model_is_validated() {
    let (m, ai) = ai_with(ScriptedModel::answers([GOOD]).labelled("local/my-model"));
    let t = ai.for_account(&account(Persona::Personal, AiPolicy::LocalOnly)).unwrap().triage(&person()).await.unwrap();
    assert_eq!(t.category, Category::Personal);
    assert_eq!(t.priority, 55);
    assert!(t.needs_reply);
    assert_eq!(t.by, "local/my-model");
    assert_eq!(t.at, NOW);
    assert_eq!(t.due, dates::parse("2026-11-13", None, 0));
    let req = &m.requests()[0];
    assert_eq!(req.task, "triage");
    assert!(req.messages[0].content.contains("2026-11-10 (Tuesday)"));
}

#[tokio::test]
async fn one_repair_turn_then_a_fallback() {
    let (m, ai) = ai_with(ScriptedModel::answers(["not json", GOOD]));
    let a = ai.for_account(&account(Persona::Personal, AiPolicy::LocalOnly)).unwrap();
    assert_eq!(a.triage(&person()).await.unwrap().category, Category::Personal);
    let r = &m.requests()[1];
    assert_eq!(r.messages.len(), 4);
    assert!(r.messages[3].content.contains("rejected"));

    let bad = r#"{"category":"sensitive","priority":5,"summary":"x","needs_reply":false,"due":null}"#;
    let (m, ai) = ai_with(ScriptedModel::answers([bad, r#"{"category":"personal","priority":500,"summary":"x","needs_reply":false,"due":null}"#]));
    let t = ai.for_account(&account(Persona::Personal, AiPolicy::LocalOnly)).unwrap().triage(&person()).await.unwrap();
    assert_eq!((t.by.as_str(), t.category, t.summary.as_str()), ("fallback", Category::Other, "Mail from Ada"));
    assert_eq!(m.calls(), 2);
}

#[tokio::test]
async fn offline_is_passed_up_for_a_later_retry() {
    let (_, ai) = ai_with(ScriptedModel::new([Err(Error::Offline("down".into()))]));
    let r = ai.for_account(&account(Persona::Personal, AiPolicy::LocalOnly)).unwrap().triage(&person()).await;
    assert!(matches!(r, Err(Error::Offline(_))));
}

#[tokio::test]
async fn implausible_dates_are_dropped_and_own_last_word_means_no_reply_needed() {
    let a1 = msg("ada@example.com", Some("Ada"), "Lunch", "Friday?", &[]);
    let mut a2 = msg(ME, Some("Sam"), "Re: Lunch", "Yes!", &[]);
    a2.meta.date += 60;
    let t = thread(vec![a1, a2]);
    let ans = r#"{"category":"personal","priority":20,"summary":"Lunch is set","needs_reply":true,"due":"1999-01-01"}"#;
    let (_, ai) = ai_with(ScriptedModel::answers([ans]));
    let tr = ai.for_account(&account(Persona::Personal, AiPolicy::LocalOnly)).unwrap().triage(&t).await.unwrap();
    assert!(!tr.needs_reply);
    assert_eq!(tr.due, None);
}

#[tokio::test]
async fn backlog_runs_rules_first_and_keeps_order() {
    let nl = thread1(msg("weekly@news.example", None, "Issue 9", "...", &[("List-Id", "<n.example>")]));
    let code = thread1(msg("no-reply@acme.example", Some("Acme"), "Your sign-in code", "Your code is 991100", &[]));
    let (m, ai) = ai_with(ScriptedModel::with(|_| Ok(GOOD.into())));
    let a = ai.for_account(&account(Persona::Personal, AiPolicy::LocalOnly)).unwrap();
    let out = a.triage_many(&[nl, person(), code]).await;
    let cats: Vec<Category> = out.into_iter().map(|r| r.unwrap().category).collect();
    assert_eq!(cats, vec![Category::Newsletter, Category::Personal, Category::Sensitive]);
    assert_eq!(m.calls(), 1);
}

#[tokio::test]
async fn summaries() {
    let (_, ai) = ai_with(ScriptedModel::answers([r#"{"summary":"Ada wants lunch Friday at noon and an answer by Thursday."}"#]));
    let s = ai.for_account(&account(Persona::Personal, AiPolicy::LocalOnly)).unwrap().summarise(&person()).await.unwrap();
    assert_eq!(s, "Ada wants lunch Friday at noon and an answer by Thursday.");
}

#[tokio::test]
async fn reply_drafts_get_recipients_and_threading_from_code() {
    let mut m = msg("ada@example.com", Some("Ada"), "Lunch on Friday?", "Free Friday?", &[("Reply-To", "ada.lists@example.com")]);
    m.meta.to = vec![Address::new(Some("Sam"), ME), Address::new(Some("Bo"), "bo@example.com")];
    m.meta.cc = vec![Address::new(None, "cy@example.com")];
    m.meta.references = vec!["r0@mail.example".into()];
    let t = thread1(m);
    let (model, ai) = ai_with(ScriptedModel::answers([r#"{"body":"Hi Ada,\n\nFriday at noon works. See you there.\n\nSam"}"#]));
    let a = ai.for_account(&account(Persona::Personal, AiPolicy::LocalOnly)).unwrap();
    let req = ReplyRequest {
        instruction: "yes to friday noon".into(),
        voice: vec!["Hi Bo,\n\nSounds good.\n\nSam".into()],
        reply_all: true,
        extra_cc: vec![Address::new(None, "Dee@Example.com")],
    };
    let d = a.draft_reply(&t, &req).await.unwrap();
    let emails = |v: &[Address]| v.iter().map(|a| a.email.clone()).collect::<Vec<_>>();
    assert_eq!(emails(&d.to), vec!["ada.lists@example.com"]);
    assert_eq!(emails(&d.cc), vec!["bo@example.com", "cy@example.com", "ada@example.com", "dee@example.com"]);
    assert!(d.bcc.is_empty());
    assert_eq!(d.subject, "Re: Lunch on Friday?");
    assert_eq!(d.in_reply_to, t.latest().meta.rfc_message_id);
    assert_eq!(d.references.len(), 2);
    assert_eq!(d.reply_to, Some(t.latest().meta.id.clone()));
    assert!(d.text.contains("Friday at noon works"));
    let user = &model.requests()[0].messages[1].content;
    assert!(user.contains("USER_INSTRUCTION") && user.contains("STYLE SAMPLES"));
}

#[tokio::test]
async fn drafts_keep_only_links_the_user_gave() {
    let ok = r#"{"body":"Here is the doc: https://docs.example.com/plan"}"#;
    let (_, ai) = ai_with(ScriptedModel::answers([ok]));
    let a = ai.for_account(&account(Persona::Personal, AiPolicy::LocalOnly)).unwrap();
    let req = ReplyRequest { instruction: "send her https://docs.example.com/plan".into(), ..Default::default() };
    assert!(a.draft_reply(&person(), &req).await.is_ok());

    let sneaky = r#"{"body":"Here is the doc: https://other.example/x"}"#;
    let (m, ai) = ai_with(ScriptedModel::answers([sneaky, r#"{"body":"Here is the doc: https://docs.example.com/plan"}"#]));
    let a = ai.for_account(&account(Persona::Personal, AiPolicy::LocalOnly)).unwrap();
    let d = a.draft_reply(&person(), &req).await.unwrap();
    assert!(!d.text.contains("other.example"));
    assert!(m.requests()[1].messages.last().unwrap().content.contains("link the user did not give"));

    let (_, ai) = ai_with(ScriptedModel::answers([sneaky, sneaky]));
    let a = ai.for_account(&account(Persona::Personal, AiPolicy::LocalOnly)).unwrap();
    assert!(matches!(a.draft_reply(&person(), &req).await, Err(Error::NoAnswer(_))), "no usable draft is its own kind");
    assert!(matches!(a.draft_reply(&person(), &ReplyRequest::default()).await, Err(Error::Invalid(_))));
}

#[tokio::test]
async fn new_message_drafts() {
    let (_, ai) = ai_with(ScriptedModel::answers([r#"{"body":"Hi Bo, are you around Thursday?"}"#]));
    let a = ai.for_account(&account(Persona::Personal, AiPolicy::LocalOnly)).unwrap();
    let d = a.draft_new(vec![Address::new(Some("Bo"), "bo@example.com")], "Thursday", "ask bo if he's around thursday", &[]).await.unwrap();
    assert_eq!(d.to[0].email, "bo@example.com");
    assert_eq!(d.subject, "Thursday");
    assert!(d.reply_to.is_none());
}

struct Store;
#[async_trait::async_trait]
impl Search for Store {
    async fn search(&self, q: &str, accounts: &[AccountId], _l: u32) -> Result<Vec<SearchHit>> {
        assert_eq!(accounts, &[AccountId::new("acct-1")]);
        let hit = |acct: &str, t: &str, from: &str, subject: &str, text: &str| SearchHit {
            thread: rpc::ThreadRef { account: AccountId::new(acct), thread: ThreadId::new(t) },
            subject: subject.into(),
            from: Address::new(None, from),
            date: 1_794_000_000,
            text: text.into(),
        };
        Ok(if q.contains("flight") {
            vec![
                hit("acct-1", "t-flight", "flights@airline.example", "Your itinerary", "Departs Dec 3 at 07:10 from SFO."),
                hit("acct-1", "t-code", "no-reply@airline.example", "Your verification code", "Your code is 123456"),
                hit("anon-1", "t-anon", "x@anon.example", "flight", "ANON SECRET"),
            ]
        } else {
            vec![]
        })
    }
}

#[tokio::test]
async fn ask_plans_searches_and_cites() {
    let (m, ai) = ai_with(ScriptedModel::with(|req| {
        Ok(match req.task {
            "ask_plan" => r#"{"queries":["flight itinerary","subject:flight"]}"#.into(),
            _ => r#"{"answer":"Your flight leaves SFO on Dec 3 at 07:10.","sources":["t1"]}"#.into(),
        })
    }));
    let a = ai.ask("when is my flight?", &[account(Persona::Personal, AiPolicy::LocalOnly)], &Store).await.unwrap();
    assert_eq!(a.text, "Your flight leaves SFO on Dec 3 at 07:10.");
    assert_eq!(a.sources, vec![rpc::ThreadRef { account: AccountId::new("acct-1"), thread: ThreadId::new("t-flight") }]);
    let reqs = m.requests();
    assert_eq!(reqs.len(), 2);
    assert!(!reqs[0].messages[1].content.contains("UNTRUSTED_MAIL"), "planning sees no mail");
    let answer_prompt = &reqs[1].messages[1].content;
    assert!(answer_prompt.contains("Departs Dec 3"));
    assert!(!answer_prompt.contains("123456"), "sensitive hits never reach the model");
    assert!(!answer_prompt.contains("ANON SECRET"), "hits from other accounts never reach the model");
}

#[tokio::test]
async fn ask_with_no_hits_needs_no_second_call_and_bad_sources_are_rejected() {
    let (m, ai) = ai_with(ScriptedModel::answers([r#"{"queries":["nothing here"]}"#]));
    let a = ai.ask("anything about yachts?", &[account(Persona::Personal, AiPolicy::LocalOnly)], &Store).await.unwrap();
    assert!(a.sources.is_empty());
    assert_eq!(m.calls(), 1);

    let bad = r#"{"answer":"Dec 3.","sources":["t7"]}"#;
    let (_, ai) = ai_with(ScriptedModel::answers([r#"{"queries":["flight"]}"#, bad, bad]));
    let a = ai.ask("flight?", &[account(Persona::Personal, AiPolicy::LocalOnly)], &Store).await.unwrap();
    assert!(a.text.starts_with("I couldn't"));
    assert_eq!(a.sources.len(), 1);
}

#[tokio::test]
async fn extraction_parses_items_and_dates() {
    let ans = r#"{"items":[{"text":"Tell Ada if Friday lunch works","owner":"me","due":"2026-11-13"},{"text":"","owner":"me","due":null}],
                 "dates":[{"what":"Lunch with Ada","date":"2026-11-14","time":"12:00"},{"what":"Bad","date":"2026-13-01","time":null}]}"#;
    let (_, ai) = ai_with(ScriptedModel::answers([ans]));
    let x = ai.for_account(&account(Persona::Personal, AiPolicy::LocalOnly)).unwrap().extract(&person()).await.unwrap();
    assert_eq!(x.items.len(), 1);
    assert_eq!(x.items[0].owner, Owner::Me);
    assert_eq!(x.items[0].due, dates::parse("2026-11-13", None, 0));
    assert_eq!(x.dates, vec![DateMention { what: "Lunch with Ada".into(), at: dates::parse("2026-11-14", Some("12:00"), 0).unwrap(), all_day: false }]);
}
