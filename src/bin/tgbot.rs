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
    /// Public https base of the built-in stream server (cloudflared quick tunnel)
    base: Arc<Mutex<Option<String>>>,
    streams: StreamMap,
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

const HELP: &str = "<b>MahSurat Stream Bot</b>\n\n\
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
        if p.is_bdix() && items.is_empty() {
            continue; // never show BDIX warnings
        }
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
    let base = ctx.base.lock().ok().and_then(|g| g.clone());
    let Some(base) = base else {
        show(
            ctx,
            chat,
            wait,
            "⏳ The player is still starting up. Please tap the quality again in a minute.",
            None,
        )
        .await;
        return;
    };
    let mut items: Vec<StreamItem> = Vec::new();
    for (i, (label, url, headers)) in links.iter().enumerate() {
        let name = if label.trim().is_empty() {
            format!("Link {}", i + 1)
        } else {
            short(label, 30)
        };
        text.push_str(&format!("\n• {}", esc(&name)));
        items.push(StreamItem {
            name,
            url: url.clone(),
            headers: headers.clone(),
        });
    }
    text.push('\n');

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

    let sid = random_id();
    let page = format!("{}/p/{}", base.trim_end_matches('/'), sid);
    let base_text = short_html(&text);
    let locked_text = format!(
        "{base_text}\n\n🔒 <b>Watch 2 short ads to unlock the Play button.</b>"
    );
    insert_stream(
        &ctx.streams,
        &sid,
        Stream {
            items,
            title: format!("{}{}", title, episode_tag(&st)),
            chat,
            msg: 0,
            text: base_text,
            page: page.clone(),
            unlocked_at: None,
        },
    );

    if let Some(w) = wait {
        let _ = ctx
            .tg
            .call("deleteMessage", &json!({"chat_id": chat, "message_id": w}))
            .await;
    }
    let back = vec![cb("⬅ Other qualities", "q".to_string())];
    let with_webapp = kb(vec![
        vec![json!({"text": "🔓 Watch 2 ads to unlock", "web_app": {"url": page.clone()}})],
        back.clone(),
    ]);
    match ctx.tg.send(chat, &locked_text, Some(with_webapp)).await {
        Ok(mid) => set_stream_msg(&ctx.streams, &sid, mid),
        Err(_) => {
            // Mini-App buttons only work in private chats; fall back to a link button.
            let with_url = kb(vec![
                vec![json!({"text": "🔓 Watch 2 ads to unlock", "url": page.clone()})],
                back,
            ]);
            if let Ok(mid) = ctx.tg.send(chat, &locked_text, Some(with_url)).await {
                set_stream_msg(&ctx.streams, &sid, mid);
            }
        }
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

// ----------------------------------------------- Built-in player/stream server

#[derive(Clone)]
struct StreamItem {
    name: String,
    url: String,
    headers: Vec<(String, String)>,
}

#[derive(Clone)]
struct Stream {
    items: Vec<StreamItem>,
    title: String,
    chat: i64,
    msg: i64,
    /// message text without the lock/unlock status line
    text: String,
    /// https://host/p/ID
    page: String,
    unlocked_at: Option<Instant>,
}

/// After watching the ads, playback stays unlocked for 6 hours.
const UNLOCK_SECS: u64 = 6 * 3600;

impl Stream {
    fn is_unlocked(&self) -> bool {
        self.unlocked_at
            .map(|t| t.elapsed() < Duration::from_secs(UNLOCK_SECS))
            .unwrap_or(false)
    }
}

type StreamMap = Arc<Mutex<HashMap<String, Stream>>>;

fn random_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    // RandomState is seeded from the OS RNG on every platform.
    let mut out = String::new();
    for i in 0u8..2 {
        let mut h = RandomState::new().build_hasher();
        h.write_u8(i);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

fn insert_stream(map: &StreamMap, id: &str, st: Stream) {
    if let Ok(mut g) = map.lock() {
        if g.len() > 5000 {
            g.clear();
        }
        g.insert(id.to_string(), st);
    }
}

fn set_stream_msg(map: &StreamMap, id: &str, msg: i64) {
    if let Ok(mut g) = map.lock() {
        if let Some(st) = g.get_mut(id) {
            st.msg = msg;
        }
    }
}

/// Play buttons that appear after the 2 ads were watched.
fn play_keyboard(st: &Stream, webapp: bool) -> Value {
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for (i, it) in st.items.iter().enumerate() {
        let page = format!("{}?n={}", st.page, i);
        let label = format!("▶️ Play: {}", short(&it.name, 30));
        if webapp {
            rows.push(vec![json!({"text": label, "web_app": {"url": page.clone()}})]);
        } else {
            rows.push(vec![json!({"text": label, "url": page.clone()})]);
        }
        rows.push(vec![
            json!({"text": "📱 MX Player", "url": format!("{page}&go=mx")}),
            json!({"text": "🎞 VLC", "url": format!("{page}&go=vlc")}),
        ]);
    }
    rows.push(vec![cb("⬅ Other qualities", "q".to_string())]);
    kb(rows)
}

/// 6 hours after unlocking: remove the Play buttons and the stream.
async fn expire_task(tg: Tg, streams: StreamMap, id: String, chat: i64, msg: i64, text: String) {
    tokio::time::sleep(Duration::from_secs(UNLOCK_SECS)).await;
    if let Ok(mut g) = streams.lock() {
        g.remove(&id);
    }
    let t = format!(
        "{text}\n\n⌛ <b>Expired.</b> Tap a quality again and watch 2 ads to play."
    );
    let _ = tg
        .edit(chat, msg, &t, Some(kb(vec![vec![cb("⬅ Other qualities", "q".to_string())]])))
        .await;
}

const PLAYER_HTML: &str = r##"<!doctype html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<title>__TITLE__</title>
<script src="https://telegram.org/js/telegram-web-app.js"></script>
<style>
html,body{margin:0;background:#0f0f14;color:#eee;font-family:system-ui,sans-serif}
h1{font-size:16px;margin:12px;font-weight:600}
#wrap{position:relative;background:#000}
video{display:block;width:100%;max-height:70vh;background:#000}
#fs{position:absolute;top:8px;right:8px;z-index:5;border:0;border-radius:8px;background:rgba(0,0,0,.55);color:#fff;font-size:18px;padding:6px 10px}
.row{display:flex;gap:8px;margin:12px}
a.b{flex:1;text-align:center;padding:12px;border-radius:10px;background:#2b6cff;color:#fff;text-decoration:none;font-weight:600}
a.v{background:#e8730c}
p{margin:12px;font-size:13px;color:#aaa}
body.fs h1,body.fs .row,body.fs p{display:none}
body.fs #wrap{position:fixed;top:0;left:0;right:0;bottom:0;z-index:99;width:100vw;height:100vh}
body.fs video{width:100vw;height:100vh;max-height:none;object-fit:contain}
</style></head><body>
<h1>__TITLE__</h1>
<div id="wrap">
<video id="v" controls controlslist="nofullscreen" autoplay playsinline webkit-playsinline src="/s/__ID__?n=__N__"></video>
<button id="fs" type="button">&#x26F6;</button>
</div>
<div class="row"><a class="b" id="mx" href="#">MX Player</a><a class="b v" id="vlc" href="#">VLC</a></div>
<p id="msg">If the video does not start here (some formats are not supported by the in-app player), use MX Player or VLC.</p>
<script>
var TITLE=__TITLE_JS__, GO="__GO__";
var tg=window.Telegram&&window.Telegram.WebApp;
if(tg){try{tg.ready();tg.expand();}catch(e){}}
var inTg=!!(tg&&tg.initData);
var abs=location.origin+"/s/__ID__?n=__N__";
function intentFor(pkg){return "intent://"+abs.replace(/^https?:\/\//,"")+"#Intent;scheme=https;package="+pkg+";type=video/*;S.title="+encodeURIComponent(TITLE)+";end";}
var PK={mx:"com.mxtech.videoplayer.ad",vlc:"org.videolan.vlc"};
function launch(k){
  if(inTg&&tg.openLink){tg.openLink(location.origin+"/p/__ID__?n=__N__&go="+k);return;}
  location.href=intentFor(PK[k]);
}
document.getElementById("mx").onclick=function(e){e.preventDefault();launch("mx");};
document.getElementById("vlc").onclick=function(e){e.preventDefault();launch("vlc");};
document.getElementById("v").addEventListener("error",function(){document.getElementById("msg").textContent="This video format cannot play in-app. Tap MX Player or VLC above.";});
/* Fullscreen: Telegram's webview blocks the native button, so use our own. */
var wrap=document.getElementById("wrap");
function fsOn(){document.body.classList.add("fs");try{if(tg&&tg.requestFullscreen)tg.requestFullscreen();}catch(e){}}
function fsOff(){document.body.classList.remove("fs");try{if(tg&&tg.exitFullscreen)tg.exitFullscreen();}catch(e){}try{if(document.fullscreenElement&&document.exitFullscreen)document.exitFullscreen();}catch(e){}}
document.getElementById("fs").onclick=function(){
  if(document.body.classList.contains("fs")){fsOff();return;}
  if(inTg){fsOn();return;}
  var r=wrap.requestFullscreen||wrap.webkitRequestFullscreen;
  if(r){try{var pr=r.call(wrap);if(pr&&pr.catch){pr.catch(function(){fsOn();});}}catch(e){fsOn();}}else{fsOn();}
};
document.addEventListener("fullscreenchange",function(){if(!document.fullscreenElement){document.body.classList.remove("fs");}});
if(GO&&PK[GO]&&!inTg){document.getElementById("v").pause();setTimeout(function(){location.href=intentFor(PK[GO]);},300);}
</script></body></html>"##;

const GATE_HTML: &str = r##"<!doctype html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Unlock</title>
<script src="https://telegram.org/js/telegram-web-app.js"></script>
<script src='//libtl.com/sdk.js' data-zone='12229158' data-sdk='show_12229158'></script>
<style>
body{margin:0;background:#0f0f14;color:#eee;font-family:system-ui,sans-serif}
h1{font-size:16px;margin:14px;font-weight:600}
.card{margin:14px;padding:18px;border-radius:14px;background:#1b1b26;text-align:center}
h2{margin:0 0 8px;font-size:18px}
p{font-size:14px;color:#bbb}
#prog{font-size:16px;margin:12px 0;font-weight:600}
button{width:100%;padding:14px;border:0;border-radius:12px;background:#2b6cff;color:#fff;font-size:16px;font-weight:700}
button:disabled{opacity:.6}
#msg{min-height:20px;color:#ffb74d}
</style></head><body>
<h1>__TITLE__</h1>
<div class="card">
<h2>&#x1F512; Unlock playback</h2>
<p>Watch 2 short ads to unlock the video. Playback stays unlocked for 6 hours.</p>
<div id="prog">Ads watched: 0 / 2</div>
<button id="btn" type="button">&#x25B6; Watch ad 1 of 2</button>
<p id="msg"></p>
</div>
<script>
var tg=window.Telegram&&window.Telegram.WebApp;
if(tg){try{tg.ready();tg.expand();}catch(e){}}
var done=0,busy=false;
var btn=document.getElementById("btn"),prog=document.getElementById("prog"),msg=document.getElementById("msg");
function adFn(){return (typeof show_12229158==="function")?show_12229158:null;}
function unlock(){
  msg.textContent="Unlocking...";
  fetch("/unlock/__ID__",{method:"POST"}).then(function(r){
    if(r.ok){location.reload();}else{msg.textContent="Could not unlock. Close this window and try again.";}
  }).catch(function(){msg.textContent="Network error. Please reopen and try again.";});
}
btn.onclick=function(){
  if(busy)return;
  var f=adFn();
  if(!f){msg.textContent="The ad could not load. Check your connection, disable any ad-blocker and try again.";return;}
  busy=true;btn.disabled=true;msg.textContent="Loading ad...";
  f().then(function(){
    done++;prog.textContent="Ads watched: "+done+" / 2";busy=false;btn.disabled=false;msg.textContent="";
    if(done>=2){btn.style.display="none";unlock();}
    else{btn.textContent="\u25B6 Watch ad "+(done+1)+" of 2";}
  }).catch(function(){busy=false;btn.disabled=false;msg.textContent="The ad was not completed. Please watch it fully.";});
};
</script></body></html>"##;

fn js_str(s: &str) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"\"".to_string())
        .replace("</", "<\\/")
}

fn player_page(id: &str, title: &str, go: &str, n: usize) -> String {
    let go = if go == "mx" || go == "vlc" { go } else { "" };
    PLAYER_HTML
        .replace("__TITLE_JS__", &js_str(title))
        .replace("__TITLE__", &esc(title))
        .replace("__ID__", id)
        .replace("__N__", &n.to_string())
        .replace("__GO__", go)
}

fn gate_page(id: &str, title: &str) -> String {
    GATE_HTML.replace("__TITLE__", &esc(title)).replace("__ID__", id)
}

async fn http_simple(
    sock: &mut tokio::net::TcpStream,
    code: u16,
    reason: &str,
    ctype: &str,
    body: &[u8],
) {
    use tokio::io::AsyncWriteExt;
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(body).await;
    let _ = sock.flush().await;
}

async fn serve_conn(
    mut sock: tokio::net::TcpStream,
    streams: StreamMap,
    http: reqwest::Client,
    tg: Tg,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        match sock.read(&mut tmp).await {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if buf.len() > 16384 {
                    return;
                }
            }
            Err(_) => return,
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("").to_string();
    let mut it = first.split_whitespace();
    let method = it.next().unwrap_or("").to_string();
    let target = it.next().unwrap_or("/").to_string();
    let mut range: Option<String> = None;
    for l in lines {
        if let Some((n, v)) = l.split_once(':') {
            if n.trim().eq_ignore_ascii_case("range") {
                range = Some(v.trim().to_string());
            }
        }
    }
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let get = |id: &str| -> Option<Stream> { streams.lock().ok().and_then(|g| g.get(id).cloned()) };

    let n: usize = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("n="))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // Called by the gate page after both ads were watched.
    if method == "POST" {
        if let Some(id) = path.strip_prefix("/unlock/") {
            let snapshot: Option<(Stream, bool)> = match streams.lock() {
                Ok(mut g) => match g.get_mut(id) {
                    Some(st) => {
                        let newly = !st.is_unlocked();
                        if newly {
                            st.unlocked_at = Some(Instant::now());
                        }
                        Some((st.clone(), newly))
                    }
                    None => None,
                },
                Err(_) => None,
            };
            let Some((st, newly)) = snapshot else {
                http_simple(&mut sock, 404, "Not Found", "text/plain", b"Expired").await;
                return;
            };
            if newly && st.msg != 0 {
                let text = format!(
                    "{}\n\n✅ <b>Unlocked for 6 hours.</b> Tap Play below.",
                    st.text
                );
                if tg
                    .edit(st.chat, st.msg, &text, Some(play_keyboard(&st, true)))
                    .await
                    .is_err()
                {
                    let _ = tg
                        .edit(st.chat, st.msg, &text, Some(play_keyboard(&st, false)))
                        .await;
                }
                tokio::spawn(expire_task(
                    tg.clone(),
                    streams.clone(),
                    id.to_string(),
                    st.chat,
                    st.msg,
                    st.text.clone(),
                ));
            }
            http_simple(&mut sock, 200, "OK", "application/json", b"{\"ok\":true}").await;
            return;
        }
    }

    if let Some(id) = path.strip_prefix("/p/") {
        let Some(entry) = get(id) else {
            http_simple(&mut sock, 404, "Not Found", "text/plain", b"Expired. Tap the quality again in the bot.").await;
            return;
        };
        let html = if entry.is_unlocked() {
            let go = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("go="))
                .unwrap_or("");
            player_page(id, &entry.title, go, n)
        } else {
            gate_page(id, &entry.title)
        };
        http_simple(&mut sock, 200, "OK", "text/html; charset=utf-8", html.as_bytes()).await;
        return;
    }

    if let Some(id) = path.strip_prefix("/s/") {
        let Some(entry) = get(id) else {
            http_simple(&mut sock, 404, "Not Found", "text/plain", b"Expired").await;
            return;
        };
        if !entry.is_unlocked() {
            http_simple(&mut sock, 403, "Forbidden", "text/plain", b"Locked: watch the ads first").await;
            return;
        }
        let Some(item) = entry.items.get(n).cloned() else {
            http_simple(&mut sock, 404, "Not Found", "text/plain", b"No such source").await;
            return;
        };
        let mut req = http.get(&item.url);
        for (hn, hv) in &item.headers {
            req = req.header(hn.as_str(), hv.as_str());
        }
        if let Some(r) = &range {
            req = req.header("Range", r.as_str());
        }
        let mut resp = match req.send().await {
            Ok(r) => r,
            Err(_) => {
                http_simple(&mut sock, 502, "Bad Gateway", "text/plain", b"upstream error").await;
                return;
            }
        };
        let status = resp.status();
        let mut out = format!(
            "HTTP/1.1 {} {}\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or("OK")
        );
        let mut has_ct = false;
        for name in [
            "content-type",
            "content-length",
            "content-range",
            "accept-ranges",
            "last-modified",
            "etag",
        ] {
            if let Some(v) = resp.headers().get(name).and_then(|v| v.to_str().ok()) {
                if name == "content-type" {
                    has_ct = true;
                }
                out.push_str(&format!("{name}: {v}\r\n"));
            }
        }
        if !has_ct {
            out.push_str("content-type: video/mp4\r\n");
        }
        out.push_str("access-control-allow-origin: *\r\nconnection: close\r\n\r\n");
        if sock.write_all(out.as_bytes()).await.is_err() {
            return;
        }
        if method == "HEAD" {
            return;
        }
        while let Ok(Some(chunk)) = resp.chunk().await {
            if sock.write_all(&chunk).await.is_err() {
                return;
            }
        }
        let _ = sock.flush().await;
        return;
    }

    http_simple(&mut sock, 200, "OK", "text/plain", b"MahSurat Stream Bot player server").await;
}

async fn server_task(listener: tokio::net::TcpListener, streams: StreamMap, tg: Tg) {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (Linux; Android 13) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Mobile Safari/537.36")
        .build()
        .unwrap_or_default();
    loop {
        match listener.accept().await {
            Ok((sock, _)) => {
                tokio::spawn(serve_conn(sock, streams.clone(), http.clone(), tg.clone()));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

fn extract_tunnel(line: &str) -> Option<String> {
    let i = line.find("https://")?;
    let rest = &line[i..];
    let suffix = ".trycloudflare.com";
    let end = rest.find(suffix)? + suffix.len();
    let url = rest[..end].to_string();
    if url.contains("://api.") {
        None
    } else {
        Some(url)
    }
}

/// Keeps a free cloudflared "quick tunnel" alive and publishes its https URL in `base`.
async fn tunnel_task(base: Arc<Mutex<Option<String>>>, port: u16) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let bin = std::env::var("CLOUDFLARED_BIN").unwrap_or_else(|_| {
        let name = if cfg!(windows) { "cloudflared.exe" } else { "cloudflared" };
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join(name)))
            .filter(|p| p.exists())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "cloudflared".to_string())
    });
    loop {
        let spawned = tokio::process::Command::new(&bin)
            .args([
                "tunnel",
                "--no-autoupdate",
                "--url",
                &format!("http://127.0.0.1:{port}"),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                eprintln!("cloudflared not available: {e}");
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
        };
        let Some(stderr) = child.stderr.take() else {
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        };
        let mut lines = BufReader::new(stderr).lines();
        let found = tokio::time::timeout(Duration::from_secs(45), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(u) = extract_tunnel(&line) {
                    return Some(u);
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        if let Some(u) = found {
            println!("Tunnel ready");
            if let Ok(mut g) = base.lock() {
                *g = Some(u);
            }
            // drain logs until the tunnel process ends
            while let Ok(Some(_)) = lines.next_line().await {}
        }
        let _ = child.kill().await;
        if let Ok(mut g) = base.lock() {
            *g = None;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ------------------------------------------------------------------- Main

fn config_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("moviebox-tgbot").join("config.txt"))
}

/// config.txt lines: token=...  allow=id1,id2
fn read_config(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(config_path()?).ok()?;
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn write_config(token: &str, allow: &str) {
    if let Some(p) = config_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, format!("token={token}\nallow={allow}\n"));
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut arg_token: Option<String> = None;
    let mut arg_allow: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--token" if i + 1 < args.len() => {
                arg_token = Some(args[i + 1].clone());
                i += 1;
            }
            "--allow" if i + 1 < args.len() => {
                arg_allow = Some(args[i + 1].clone());
                i += 1;
            }
            "--help" | "-h" => {
                println!("moviebox-tgbot [--token BOT_TOKEN] [--allow CHAT_ID,CHAT_ID]\n\nThe token and allowed ids are saved, so next time just run: moviebox-tgbot");
                return;
            }
            _ => {}
        }
        i += 1;
    }

    let env_token = std::env::var("BOT_TOKEN").ok().filter(|t| !t.trim().is_empty());
    let mut token = arg_token.or(env_token).or_else(|| read_config("token"));
    if token.is_none() {
        use std::io::{BufRead, IsTerminal, Write};
        if std::io::stdin().is_terminal() {
            print!("Paste your Telegram bot token (from @BotFather): ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            let t = line.trim().to_string();
            if !t.is_empty() {
                token = Some(t);
            }
        }
    }
    let token = match token {
        Some(t) => t.trim().to_string(),
        None => {
            eprintln!("BOT_TOKEN is not set (use --token or the BOT_TOKEN variable)");
            std::process::exit(1);
        }
    };
    let allow_raw = arg_allow
        .or_else(|| std::env::var("ALLOWED_CHAT_IDS").ok().filter(|v| !v.trim().is_empty()))
        .or_else(|| read_config("allow"))
        .unwrap_or_default();
    let on_github = std::env::var("GITHUB_ACTIONS").is_ok();
    if !on_github {
        write_config(&token, &allow_raw);
    }
    let allowed: Vec<i64> = allow_raw
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();
    // On GitHub the run window is limited; on your own PC/phone it runs until you stop it.
    let run_minutes: Option<u64> = std::env::var("RUN_MINUTES")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .or(if on_github { Some(345) } else { None });

    let ctx = Ctx {
        tg: Tg::new(&token),
        svc: MovieBoxService::new(),
        states: Arc::new(Mutex::new(HashMap::new())),
        allowed: Arc::new(allowed),
        base: Arc::new(Mutex::new(None)),
        streams: Arc::new(Mutex::new(HashMap::new())),
    };

    match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => {
            let port = listener.local_addr().map(|a| a.port()).unwrap_or(8787);
            tokio::spawn(server_task(listener, ctx.streams.clone(), ctx.tg.clone()));
            tokio::spawn(tunnel_task(ctx.base.clone(), port));
        }
        Err(e) => eprintln!("player server failed to start: {e}"),
    }

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

    println!("Bot started ({})", if on_github { "GitHub Actions" } else { "local" });
    let deadline: Option<Instant> = run_minutes.map(|m| Instant::now() + Duration::from_secs(m * 60));
    let mut offset: i64 = 0;

    while deadline.is_none_or(|d| Instant::now() < d) {
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
                if e.contains("Conflict") {
                    // Another instance (GitHub vs your PC/phone) is polling the same bot.
                    // The GitHub copy steps aside so your local BDIX-capable copy wins.
                    let wait = if on_github { 300 } else { 2 };
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                } else {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
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
