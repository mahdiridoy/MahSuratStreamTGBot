//! Telegram bot front-end for the MovieBox-TUI provider library.
//!
//! Flow: search all providers -> pick provider -> pick title -> (season/episode)
//! -> pick audio -> pick quality/source -> receive stream/download links.
//!
//! Runs with long polling (no server needed), so it works inside GitHub Actions.
//!
//! Env:
//!   BOT_TOKEN          Telegram bot token (required)
//!   ALLOWED_CHAT_IDS   Optional comma-separated chat ids allowed to use the bot
//!   RUN_MINUTES        Optional, exit gracefully after N minutes (default 350)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use moviebox_tui::providers::ReleaseProvider;
use moviebox_tui::providers::models::{
    CatalogItem, MediaDetails, ProviderError, ProviderKind, Release, ResolutionIntent,
};
use moviebox_tui::service::MovieBoxService;
use serde_json::{Value, json};

const PAGE_ITEMS: usize = 8;
const PAGE_EPISODES: usize = 30;
const MAX_QUALITY_BUTTONS: usize = 40;

// ---------------------------------------------------------------- Telegram

#[derive(Clone)]
struct Tg {
    http: reqwest::Client,
    base: String,
}

impl Tg {
    fn new(token: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self {
            http,
            base: format!("https://api.telegram.org/bot{token}"),
        }
    }

    async fn call(&self, method: &str, body: &Value) -> Result<Value, String> {
        let resp = self
            .http
            .post(format!("{}/{}", self.base, method))
            .timeout(Duration::from_secs(70))
            .json(body)
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        let v: Value = resp.json().await.map_err(|e| e.without_url().to_string())?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(v.get("result").cloned().unwrap_or(Value::Null))
        } else {
            Err(v
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("telegram error")
                .to_string())
        }
    }

    async fn send(&self, chat: i64, text: &str, kb: Option<Value>) -> Result<i64, String> {
        let mut body = json!({
            "chat_id": chat,
            "text": text,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true}
        });
        if let Some(k) = kb {
            body["reply_markup"] = k;
        }
        let r = self.call("sendMessage", &body).await?;
        Ok(r.get("message_id").and_then(Value::as_i64).unwrap_or(0))
    }

    async fn edit(&self, chat: i64, msg: i64, text: &str, kb: Option<Value>) -> Result<(), String> {
        let mut body = json!({
            "chat_id": chat,
            "message_id": msg,
            "text": text,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true}
        });
        if let Some(k) = kb {
            body["reply_markup"] = k;
        }
        self.call("editMessageText", &body).await.map(|_| ())
    }

    async fn photo(&self, chat: i64, url: &str, caption: &str) -> Result<(), String> {
        let body = json!({
            "chat_id": chat,
            "photo": url,
            "caption": caption,
            "parse_mode": "HTML"
        });
        self.call("sendPhoto", &body).await.map(|_| ())
    }

    async fn answer(&self, callback_id: &str, text: &str) {
        let _ = self
            .call(
                "answerCallbackQuery",
                &json!({"callback_query_id": callback_id, "text": text}),
            )
            .await;
    }
}

// ------------------------------------------------------------------ State

#[derive(Default, Clone)]
struct ChatState {
    query: String,
    results: Vec<(ProviderKind, Vec<CatalogItem>, Option<String>)>,
    provider: ProviderKind,
    details: Option<MediaDetails>,
    subject_id: String,
    dub_chosen: bool,
    season: usize,
    episode: usize,
    releases: Vec<Release>,
    lang: Option<String>,
}

#[derive(Clone)]
struct Ctx {
    tg: Tg,
    svc: MovieBoxService,
    states: Arc<Mutex<HashMap<i64, ChatState>>>,
    allowed: Arc<Vec<i64>>,
}

fn st_get(ctx: &Ctx, chat: i64) -> ChatState {
    ctx.states
        .lock()
        .map(|g| g.get(&chat).cloned().unwrap_or_default())
        .unwrap_or_default()
}

fn st_set<F: FnOnce(&mut ChatState)>(ctx: &Ctx, chat: i64, f: F) {
    if let Ok(mut g) = ctx.states.lock() {
        f(g.entry(chat).or_default());
    }
}

// ---------------------------------------------------------------- Helpers

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn fmt_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1_073_741_824.0 {
        format!("{:.2} GB", b / 1_073_741_824.0)
    } else {
        format!("{:.0} MB", b / 1_048_576.0)
    }
}

fn cb(text: &str, data: String) -> Value {
    json!({"text": text, "callback_data": data})
}

fn kb(rows: Vec<Vec<Value>>) -> Value {
    json!({"inline_keyboard": rows})
}

fn rel_label(r: &Release) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(r.quality.clone().unwrap_or_else(|| "Auto".to_string()));
    if let Some(sz) = r.size_bytes {
        parts.push(fmt_size(sz));
    }
    if let Some(l) = r.language.as_ref().filter(|l| !l.trim().is_empty()) {
        parts.push(l.trim().to_string());
    }
    if let Some(c) = r.codec.as_ref().filter(|c| !c.trim().is_empty()) {
        parts.push(c.trim().to_string());
    }
    parts.push(r.source_label().to_string());
    short(&parts.join(" • "), 60)
}

fn episode_tag(st: &ChatState) -> String {
    if st.season > 0 {
        format!(" S{:02}E{:02}", st.season, st.episode)
    } else {
        String::new()
    }
}

async fn show(ctx: &Ctx, chat: i64, msg: Option<i64>, text: &str, markup: Option<Value>) {
    match msg {
        Some(m) => {
            if let Err(e) = ctx.tg.edit(chat, m, text, markup.clone()).await {
                if !e.contains("not modified") {
                    let _ = ctx.tg.send(chat, text, markup).await;
                }
            }
        }
        None => {
            let _ = ctx.tg.send(chat, text, markup).await;
        }
    }
}

const HELP: &str = "<b>MovieBox Bot</b>\n\n\
Send a movie or series name (or use /search name).\n\
I search <b>all providers</b>, then you pick:\n\
1. Provider\n2. Title\n3. Season / Episode (series)\n4. Audio language\n5. Quality / source\n\n\
You get direct stream/download links + subtitles when available.\n\n\
/search &lt;name&gt; - search all providers\n/id - show your chat id";

// ----------------------------------------------------------------- Search

async fn do_search(ctx: &Ctx, chat: i64, query: &str) {
    let wait = ctx
        .tg
        .send(chat, &format!("🔎 Searching all providers for <b>{}</b>…", esc(query)), None)
        .await
        .ok();

    let futs = ProviderKind::ENABLED.into_iter().map(|p| {
        let svc = ctx.svc.clone();
        let q = query.to_string();
        async move {
            let r = tokio::time::timeout(Duration::from_secs(25), svc.search_typed(p, &q, 1)).await;
            (p, r)
        }
    });
    let outs = futures::future::join_all(futs).await;

    let mut results = Vec::new();
    for (p, r) in outs {
        let (items, err) = match r {
            Ok(Ok(v)) => (v, None),
            Ok(Err(ProviderError::NotFound)) => (Vec::new(), None),
            Ok(Err(e)) => (Vec::new(), Some(e.user_message(p))),
            Err(_) => (Vec::new(), Some(format!("{} timed out.", p.label()))),
        };
        results.push((p, items, err));
    }
    let q = query.to_string();
    st_set(ctx, chat, |s| {
        *s = ChatState::default();
        s.query = q;
        s.results = results;
    });
    show_providers(ctx, chat, wait).await;
}

async fn show_providers(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let st = st_get(ctx, chat);
    let mut text = format!("Results for <b>{}</b>:\n", esc(&st.query));
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut any = false;
    for (i, (p, items, err)) in st.results.iter().enumerate() {
        if !items.is_empty() {
            any = true;
            text.push_str(&format!("\n✅ {}: {} result(s)", p.label(), items.len()));
            rows.push(vec![cb(
                &format!("{} ({})", p.label(), items.len()),
                format!("p:{i}:0"),
            )]);
        } else if let Some(e) = err {
            text.push_str(&format!("\n⚠️ {}: {}", p.label(), esc(e)));
        } else {
            text.push_str(&format!("\n▫️ {}: nothing found", p.label()));
        }
    }
    if any {
        text.push_str("\n\nChoose a provider:");
    } else {
        text.push_str("\n\nNo results. Try another name.");
    }
    let markup = if rows.is_empty() { None } else { Some(kb(rows)) };
    show(ctx, chat, msg, &text, markup).await;
}

async fn show_items(ctx: &Ctx, chat: i64, msg: Option<i64>, pi: usize, page: usize) {
    let st = st_get(ctx, chat);
    let Some((p, items, _)) = st.results.get(pi) else {
        expired(ctx, chat).await;
        return;
    };
    let total = items.len();
    let start = (page * PAGE_ITEMS).min(total);
    let end = (start + PAGE_ITEMS).min(total);
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for (j, it) in items.iter().enumerate().take(end).skip(start) {
        let icon = if it.media_type == moviebox_tui::providers::models::MediaType::Series {
            "📺"
        } else {
            "🎬"
        };
        let year = it
            .year
            .as_ref()
            .filter(|y| !y.is_empty())
            .map(|y| format!(" ({y})"))
            .unwrap_or_default();
        rows.push(vec![cb(
            &short(&format!("{icon} {}{year}", it.title), 56),
            format!("d:{pi}:{j}"),
        )]);
    }
    let mut nav = Vec::new();
    if page > 0 {
        nav.push(cb("◀ Prev", format!("p:{pi}:{}", page - 1)));
    }
    if end < total {
        nav.push(cb("Next ▶", format!("p:{pi}:{}", page + 1)));
    }
    if !nav.is_empty() {
        rows.push(nav);
    }
    rows.push(vec![cb("⬅ Providers", "pv".to_string())]);
    let text = format!(
        "<b>{}</b> — {}-{} of {}\nPick a title:",
        esc(p.label()),
        start + 1,
        end,
        total
    );
    show(ctx, chat, msg, &text, Some(kb(rows))).await;
}

async fn expired(ctx: &Ctx, chat: i64) {
    let _ = ctx
        .tg
        .send(chat, "Session expired. Please search again.", None)
        .await;
}

// ---------------------------------------------------------------- Details

fn card(d: &MediaDetails, provider: ProviderKind) -> String {
    let mut s = format!("<b>{}</b>", esc(&d.title));
    if let Some(y) = d.year.as_ref().filter(|y| !y.is_empty()) {
        s.push_str(&format!(" ({})", esc(y)));
    }
    s.push_str(&format!(
        "\n{} • {}",
        if d.is_series() { "Series" } else { "Movie" },
        esc(provider.label())
    ));
    if let Some(r) = d.imdb_rating.as_ref().filter(|r| !r.is_empty()) {
        s.push_str(&format!(" • ⭐ {}", esc(r)));
    }
    if let Some(r) = d.duration.as_ref().filter(|r| !r.is_empty()) {
        s.push_str(&format!(" • {}", esc(r)));
    }
    if !d.genres.is_empty() {
        s.push_str(&format!("\n{}", esc(&d.genres.join(", "))));
    }
    if let Some(a) = d.audios.as_ref().filter(|a| !a.is_empty()) {
        s.push_str(&format!("\n🔊 {}", esc(&short(a, 120))));
    }
    if let Some(a) = d.stars.as_ref().filter(|a| !a.is_empty()) {
        s.push_str(&format!("\n👥 {}", esc(&short(a, 120))));
    }
    if let Some(desc) = d.description.as_ref().filter(|a| !a.is_empty()) {
        let used = s.chars().count();
        let budget = 940usize.saturating_sub(used);
        if budget > 40 {
            s.push_str(&format!("\n\n{}", esc(&short(desc, budget))));
        }
    }
    s
}

async fn open_item(ctx: &Ctx, chat: i64, pi: usize, j: usize) {
    let st = st_get(ctx, chat);
    let Some((p, items, _)) = st.results.get(pi) else {
        expired(ctx, chat).await;
        return;
    };
    let Some(item) = items.get(j).cloned() else {
        expired(ctx, chat).await;
        return;
    };
    let p = *p;
    let wait = ctx.tg.send(chat, "⏳ Loading details…", None).await.ok();

    let res = tokio::time::timeout(
        Duration::from_secs(30),
        ctx.svc.details_typed(p, &item.id.value),
    )
    .await;
    let details = match res {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => {
            show(ctx, chat, wait, &format!("⚠️ {}", esc(&e.user_message(p))), None).await;
            return;
        }
        Err(_) => {
            show(ctx, chat, wait, "⚠️ Timed out loading details.", None).await;
            return;
        }
    };

    let caption = card(&details, p);
    let poster = details
        .poster_url
        .clone()
        .or(item.poster_url.clone())
        .filter(|u| u.starts_with("http"));
    let d2 = details.clone();
    st_set(ctx, chat, |s| {
        s.provider = p;
        s.subject_id = d2.id.value.clone();
        s.details = Some(d2);
        s.dub_chosen = false;
        s.season = 0;
        s.episode = 0;
        s.releases.clear();
        s.lang = None;
    });

    let mut sent_photo = false;
    if let Some(url) = poster {
        sent_photo = ctx.tg.photo(chat, &url, &caption).await.is_ok();
    }
    if !sent_photo {
        let _ = ctx.tg.send(chat, &caption, None).await;
    }
    if let Some(w) = wait {
        let _ = ctx.tg.call("deleteMessage", &json!({"chat_id": chat, "message_id": w})).await;
    }

    if details.is_series() && !details.seasons.is_empty() {
        show_seasons(ctx, chat, None).await;
    } else {
        proceed(ctx, chat, None).await;
    }
}

async fn show_seasons(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let st = st_get(ctx, chat);
    let Some(d) = st.details.as_ref() else {
        expired(ctx, chat).await;
        return;
    };
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut row: Vec<Value> = Vec::new();
    for s in d.seasons.iter().take(40) {
        row.push(cb(&format!("Season {}", s.number), format!("s:{}:0", s.number)));
        if row.len() == 4 {
            rows.push(std::mem::take(&mut row));
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    show(ctx, chat, msg, "Choose a season:", Some(kb(rows))).await;
}

async fn show_episodes(ctx: &Ctx, chat: i64, msg: Option<i64>, season: usize, page: usize) {
    let st = st_get(ctx, chat);
    let Some(d) = st.details.as_ref() else {
        expired(ctx, chat).await;
        return;
    };
    let Some(se) = d.seasons.iter().find(|s| s.number == season) else {
        expired(ctx, chat).await;
        return;
    };
    let total = se.episodes.len();
    let start = (page * PAGE_EPISODES).min(total);
    let end = (start + PAGE_EPISODES).min(total);
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut row: Vec<Value> = Vec::new();
    for ep in se.episodes.iter().take(end).skip(start) {
        row.push(cb(
            &format!("E{}", ep.number),
            format!("e:{}:{}", season, ep.number),
        ));
        if row.len() == 5 {
            rows.push(std::mem::take(&mut row));
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    let mut nav = Vec::new();
    if page > 0 {
        nav.push(cb("◀ Prev", format!("s:{season}:{}", page - 1)));
    }
    if end < total {
        nav.push(cb("Next ▶", format!("s:{season}:{}", page + 1)));
    }
    if !nav.is_empty() {
        rows.push(nav);
    }
    rows.push(vec![cb("⬅ Seasons", "ss".to_string())]);
    let text = format!("<b>Season {season}</b> — choose an episode ({total} total):");
    show(ctx, chat, msg, &text, Some(kb(rows))).await;
}

// ------------------------------------------------- Audio / streams / quality

/// After the title (movie) or episode is chosen: ask for audio dub if the
/// provider exposes several, otherwise go straight to fetching streams.
async fn proceed(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let st = st_get(ctx, chat);
    let Some(d) = st.details.as_ref() else {
        expired(ctx, chat).await;
        return;
    };
    if d.has_languages() && !st.dub_chosen {
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for (k, dub) in d.dubs.iter().enumerate().take(40) {
            let name = if dub.label.trim().is_empty() {
                dub.language.clone()
            } else {
                dub.label.clone()
            };
            rows.push(vec![cb(&format!("🔊 {}", short(&name, 40)), format!("a:{k}"))]);
        }
        show(ctx, chat, msg, "Choose the audio / dub:", Some(kb(rows))).await;
        return;
    }
    fetch_releases(ctx, chat, msg).await;
}

async fn provider_streams(
    svc: &MovieBoxService,
    p: ProviderKind,
    id: &str,
    season: usize,
    episode: usize,
) -> Result<Vec<Release>, ProviderError> {
    match p {
        ProviderKind::MovieBox => {
            ReleaseProvider::episode_streams(&svc.client, id, season, episode).await
        }
        ProviderKind::FourKHdHub => match svc.fourk_client.as_ref() {
            Some(c) => ReleaseProvider::episode_streams(c, id, season, episode).await,
            None => Err(ProviderError::Unavailable("4KHDHub is unavailable".to_string())),
        },
        ProviderKind::Dramachi => {
            ReleaseProvider::episode_streams(&svc.dramachi_client, id, season, episode).await
        }
        ProviderKind::BdixCircleFtp => {
            ReleaseProvider::episode_streams(&svc.circleftp_client, id, season, episode).await
        }
        ProviderKind::BdixDhakaFlix => {
            ReleaseProvider::episode_streams(&svc.dhakaflix_client, id, season, episode).await
        }
        ProviderKind::Addons => Err(ProviderError::Unavailable(
            "Addons are not supported in the bot".to_string(),
        )),
    }
}

async fn fetch_releases(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let st = st_get(ctx, chat);
    let target = match msg {
        Some(m) => {
            let _ = ctx.tg.edit(chat, m, "⏳ Fetching streams…", None).await;
            Some(m)
        }
        None => ctx.tg.send(chat, "⏳ Fetching streams…", None).await.ok(),
    };
    let p = st.provider;
    let res = tokio::time::timeout(
        Duration::from_secs(45),
        provider_streams(&ctx.svc, p, &st.subject_id, st.season, st.episode),
    )
    .await;
    let mut releases = match res {
        Ok(Ok(r)) if !r.is_empty() => r,
        Ok(Ok(_)) => {
            show(ctx, chat, target, "⚠️ No streams found for this item.", None).await;
            return;
        }
        Ok(Err(e)) => {
            show(ctx, chat, target, &format!("⚠️ {}", esc(&e.user_message(p))), None).await;
            return;
        }
        Err(_) => {
            show(ctx, chat, target, "⚠️ Timed out fetching streams.", None).await;
            return;
        }
    };
    releases.sort_by(|a, b| {
        b.resolution_u64()
            .cmp(&a.resolution_u64())
            .then_with(|| b.size_bytes.cmp(&a.size_bytes))
    });
    st_set(ctx, chat, |s| {
        s.releases = releases;
        s.lang = None;
    });
    after_releases(ctx, chat, target).await;
}

fn languages(releases: &[Release]) -> Vec<String> {
    let mut langs: Vec<String> = releases
        .iter()
        .filter_map(|r| r.language.clone())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    langs.sort_by_key(|s| s.to_lowercase());
    langs.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    langs
}

async fn after_releases(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let st = st_get(ctx, chat);
    if languages(&st.releases).len() > 1 {
        show_languages(ctx, chat, msg).await;
    } else {
        show_qualities(ctx, chat, msg).await;
    }
}

async fn show_languages(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let st = st_get(ctx, chat);
    let langs = languages(&st.releases);
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for (k, l) in langs.iter().enumerate().take(40) {
        let n = st
            .releases
            .iter()
            .filter(|r| {
                r.language
                    .as_deref()
                    .is_some_and(|x| x.trim().eq_ignore_ascii_case(l))
            })
            .count();
        rows.push(vec![cb(
            &format!("🔊 {} ({n})", short(l, 36)),
            format!("l:{k}"),
        )]);
    }
    rows.push(vec![cb("All audio languages", "l:x".to_string())]);
    show(ctx, chat, msg, "Choose the audio language:", Some(kb(rows))).await;
}

async fn show_qualities(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let st = st_get(ctx, chat);
    let mut idx: Vec<usize> = st
        .releases
        .iter()
        .enumerate()
        .filter(|(_, r)| match &st.lang {
            None => true,
            Some(l) => r
                .language
                .as_deref()
                .is_some_and(|x| x.trim().eq_ignore_ascii_case(l)),
        })
        .map(|(i, _)| i)
        .collect();
    if idx.is_empty() {
        idx = (0..st.releases.len()).collect();
    }
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for i in idx.iter().take(MAX_QUALITY_BUTTONS) {
        rows.push(vec![cb(&rel_label(&st.releases[*i]), format!("r:{i}"))]);
    }
    if languages(&st.releases).len() > 1 {
        rows.push(vec![cb("🔊 Change audio", "lm".to_string())]);
    }
    let title = st
        .details
        .as_ref()
        .map(|d| d.title.clone())
        .unwrap_or_default();
    let lang = st
        .lang
        .as_ref()
        .map(|l| format!(" • {}", esc(l)))
        .unwrap_or_default();
    let text = format!(
        "<b>{}</b>{}{}\nChoose quality / source ({} available):",
        esc(&title),
        esc(&episode_tag(&st)),
        lang,
        idx.len()
    );
    show(ctx, chat, msg, &text, Some(kb(rows))).await;
}

// ------------------------------------------------------------------ Links

async fn send_links(ctx: &Ctx, chat: i64, k: usize) {
    let st = st_get(ctx, chat);
    let Some(rel) = st.releases.get(k).cloned() else {
        expired(ctx, chat).await;
        return;
    };
    let wait = ctx.tg.send(chat, "⏳ Getting links…", None).await.ok();

    // (label, url, headers)
    let mut links: Vec<(String, String, Vec<(String, String)>)> = Vec::new();
    let mut problem: Option<String> = None;

    if rel.provider == ProviderKind::FourKHdHub {
        match ctx.svc.fourk_client.as_ref() {
            Some(c) => {
                let r = tokio::time::timeout(
                    Duration::from_secs(30),
                    c.resolve_release(&rel, ResolutionIntent::Download),
                )
                .await;
                match r {
                    Ok(Ok(src)) => links.push((src.source_label, src.url, src.headers)),
                    Ok(Err(e)) => problem = Some(e.user_message().to_string()),
                    Err(_) => problem = Some("Link resolver timed out.".to_string()),
                }
            }
            None => problem = Some("4KHDHub is unavailable.".to_string()),
        }
    } else {
        for m in rel.mirrors.iter().take(6) {
            links.push((m.label.clone(), m.resolver_url.clone(), m.headers.clone()));
        }
        if links.is_empty() {
            problem = Some("This release has no links.".to_string());
        }
    }

    if links.is_empty() {
        show(
            ctx,
            chat,
            wait,
            &format!("⚠️ {}", esc(&problem.unwrap_or_else(|| "No links.".to_string()))),
            None,
        )
        .await;
        return;
    }

    let title = st
        .details
        .as_ref()
        .map(|d| d.title.clone())
        .unwrap_or_default();
    let mut text = format!(
        "<b>{}</b>{}\n{}\n",
        esc(&title),
        esc(&episode_tag(&st)),
        esc(&rel_label(&rel))
    );
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for (i, (label, url, headers)) in links.iter().enumerate() {
        let name = if label.trim().is_empty() {
            format!("Link {}", i + 1)
        } else {
            short(label, 30)
        };
        text.push_str(&format!("\n<b>{}</b>\n<code>{}</code>", esc(&name), esc(url)));
        if !headers.is_empty() {
            let h: Vec<String> = headers
                .iter()
                .filter(|(n, _)| {
                    n.eq_ignore_ascii_case("referer") || n.eq_ignore_ascii_case("user-agent")
                })
                .map(|(n, v)| format!("{n}: {}", short(v, 120)))
                .collect();
            if !h.is_empty() {
                text.push_str(&format!(
                    "\n<i>needs headers:</i> <code>{}</code>",
                    esc(&h.join(" | "))
                ));
            }
        }
        text.push('\n');
        if url.starts_with("http") {
            rows.push(vec![json!({"text": format!("▶️ Open: {name}"), "url": url})]);
        }
    }

    // Subtitles (MovieBox)
    if st.provider == ProviderKind::MovieBox {
        if let (Some(rid), Some(d)) = (rel.resource_id.as_ref(), st.details.as_ref()) {
            let caps = tokio::time::timeout(
                Duration::from_secs(15),
                ctx.svc
                    .get_ext_captions(&st.subject_id, rid, &d.sibling_ids(), st.season, st.episode),
            )
            .await;
            if let Ok(Ok(list)) = caps {
                if !list.is_empty() {
                    text.push_str("\n<b>Subtitles</b>\n");
                    for s in list.iter().take(12) {
                        text.push_str(&format!(
                            "• <a href=\"{}\">{}</a>\n",
                            esc(&s.url).replace('"', "%22"),
                            esc(&short(&s.name, 40))
                        ));
                    }
                }
            }
        }
    }

    text.push_str("\n<i>Tip: open the link in VLC / MX Player (Network stream) for best results.</i>");
    rows.push(vec![cb("⬅ Other qualities", "q".to_string())]);
    let markup = kb(rows);

    if let Some(w) = wait {
        let _ = ctx
            .tg
            .call("deleteMessage", &json!({"chat_id": chat, "message_id": w}))
            .await;
    }
    let text = short_html(&text);
    if ctx.tg.send(chat, &text, Some(markup)).await.is_err() {
        let plain = json!({"inline_keyboard": [[cb("⬅ Other qualities", "q".to_string())]]});
        let _ = ctx.tg.send(chat, &text, Some(plain)).await;
    }
}

/// Telegram messages are limited to 4096 chars.
fn short_html(s: &str) -> String {
    if s.chars().count() <= 4000 {
        s.to_string()
    } else {
        let t: String = s.chars().take(3990).collect();
        // avoid cutting inside an HTML tag/entity
        let cut = t.rfind('\n').unwrap_or(t.len());
        t[..cut].to_string()
    }
}

// --------------------------------------------------------------- Dispatch

fn authorised(ctx: &Ctx, chat: i64) -> bool {
    ctx.allowed.is_empty() || ctx.allowed.contains(&chat)
}

async fn handle_message(ctx: Ctx, msg: Value) {
    let Some(chat) = msg.pointer("/chat/id").and_then(Value::as_i64) else {
        return;
    };
    let text = msg.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if text.is_empty() {
        return;
    }
    if text.starts_with("/id") {
        let _ = ctx.tg.send(chat, &format!("Chat id: <code>{chat}</code>"), None).await;
        return;
    }
    if !authorised(&ctx, chat) {
        let _ = ctx.tg.send(chat, "⛔ You are not allowed to use this bot.", None).await;
        return;
    }
    if text.starts_with('/') {
        let (cmd, rest) = match text.split_once(char::is_whitespace) {
            Some((c, r)) => (c.to_string(), r.trim().to_string()),
            None => (text.clone(), String::new()),
        };
        let cmd = cmd.split('@').next().unwrap_or("").to_lowercase();
        match cmd.as_str() {
            "/start" | "/help" => {
                let _ = ctx.tg.send(chat, HELP, None).await;
            }
            "/search" | "/s" => {
                if rest.is_empty() {
                    let _ = ctx.tg.send(chat, "Usage: /search &lt;movie or series name&gt;", None).await;
                } else {
                    do_search(&ctx, chat, &rest).await;
                }
            }
            _ => {
                let _ = ctx.tg.send(chat, HELP, None).await;
            }
        }
        return;
    }
    do_search(&ctx, chat, &text).await;
}

async fn handle_callback(ctx: Ctx, cq: Value) {
    let id = cq.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    ctx.tg.answer(&id, "").await;
    let Some(chat) = cq.pointer("/message/chat/id").and_then(Value::as_i64) else {
        return;
    };
    if !authorised(&ctx, chat) {
        return;
    }
    let msg_id = cq.pointer("/message/message_id").and_then(Value::as_i64);
    let data = cq.get("data").and_then(Value::as_str).unwrap_or("").to_string();
    let parts: Vec<&str> = data.split(':').collect();
    let num = |i: usize| parts.get(i).and_then(|s| s.parse::<usize>().ok());

    match parts.first().copied().unwrap_or("") {
        "pv" => show_providers(&ctx, chat, msg_id).await,
        "p" => {
            if let (Some(pi), Some(page)) = (num(1), num(2)) {
                show_items(&ctx, chat, msg_id, pi, page).await;
            }
        }
        "d" => {
            if let (Some(pi), Some(j)) = (num(1), num(2)) {
                open_item(&ctx, chat, pi, j).await;
            }
        }
        "ss" => show_seasons(&ctx, chat, msg_id).await,
        "s" => {
            if let (Some(season), Some(page)) = (num(1), num(2)) {
                show_episodes(&ctx, chat, msg_id, season, page).await;
            }
        }
        "e" => {
            if let (Some(season), Some(ep)) = (num(1), num(2)) {
                st_set(&ctx, chat, |s| {
                    s.season = season;
                    s.episode = ep;
                    s.dub_chosen = false;
                    s.releases.clear();
                    s.lang = None;
                });
                proceed(&ctx, chat, msg_id).await;
            }
        }
        "a" => {
            if let Some(k) = num(1) {
                st_set(&ctx, chat, |s| {
                    if let Some(sid) = s
                        .details
                        .as_ref()
                        .and_then(|d| d.dubs.get(k))
                        .map(|d| d.subject_id.clone())
                        .filter(|x| !x.is_empty())
                    {
                        s.subject_id = sid;
                    }
                    s.dub_chosen = true;
                });
                fetch_releases(&ctx, chat, msg_id).await;
            }
        }
        "l" => {
            let st = st_get(&ctx, chat);
            let langs = languages(&st.releases);
            let choice = num(1).and_then(|k| langs.get(k).cloned());
            st_set(&ctx, chat, |s| s.lang = choice);
            show_qualities(&ctx, chat, msg_id).await;
        }
        "lm" => show_languages(&ctx, chat, msg_id).await,
        "q" => show_qualities(&ctx, chat, None).await,
        "r" => {
            if let Some(k) = num(1) {
                send_links(&ctx, chat, k).await;
            }
        }
        _ => {}
    }
}

async fn handle_update(ctx: Ctx, upd: Value) {
    if let Some(m) = upd.get("message").cloned() {
        handle_message(ctx, m).await;
    } else if let Some(c) = upd.get("callback_query").cloned() {
        handle_callback(ctx, c).await;
    }
}

// ------------------------------------------------------------------- Main

#[tokio::main]
async fn main() {
    let token = match std::env::var("BOT_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            eprintln!("BOT_TOKEN is not set");
            std::process::exit(1);
        }
    };
    let allowed: Vec<i64> = std::env::var("ALLOWED_CHAT_IDS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();
    let run_minutes: u64 = std::env::var("RUN_MINUTES")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(350);

    let ctx = Ctx {
        tg: Tg::new(&token),
        svc: MovieBoxService::new(),
        states: Arc::new(Mutex::new(HashMap::new())),
        allowed: Arc::new(allowed),
    };

    let _ = ctx
        .tg
        .call("deleteWebhook", &json!({"drop_pending_updates": false}))
        .await;
    let _ = ctx
        .tg
        .call(
            "setMyCommands",
            &json!({"commands": [
                {"command": "search", "description": "Search all providers"},
                {"command": "help", "description": "How to use the bot"},
                {"command": "id", "description": "Show your chat id"}
            ]}),
        )
        .await;

    println!("Bot started; running for {run_minutes} minutes");
    let deadline = Instant::now() + Duration::from_secs(run_minutes * 60);
    let mut offset: i64 = 0;

    while Instant::now() < deadline {
        let body = json!({
            "offset": offset,
            "timeout": 30,
            "allowed_updates": ["message", "callback_query"]
        });
        match ctx.tg.call("getUpdates", &body).await {
            Ok(v) => {
                if let Some(arr) = v.as_array() {
                    for u in arr {
                        if let Some(id) = u.get("update_id").and_then(Value::as_i64) {
                            offset = id + 1;
                        }
                        tokio::spawn(handle_update(ctx.clone(), u.clone()));
                    }
                }
            }
            Err(e) => {
                eprintln!("getUpdates error: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    // Give in-flight handlers a moment, then acknowledge processed updates
    // so the next run does not repeat them.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let _ = ctx
        .tg
        .call("getUpdates", &json!({"offset": offset, "timeout": 0}))
        .await;
    println!("Run window finished; exiting");
}
