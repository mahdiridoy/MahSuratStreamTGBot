//! Telegram bot front-end for the MovieBox-TUI provider library.
//!
//! Flow: search all providers -> one combined result list -> pick title -> (season/episode)
//! -> All audio languages -> pick quality/source (all providers merged) -> play / download.
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
    CatalogItem, MediaDetails, MediaType, ProviderError, ProviderKind, Release, ResolutionIntent,
};
use moviebox_tui::service::MovieBoxService;
use serde_json::{Value, json};

const PAGE_ITEMS: usize = 10;
const PAGE_EPISODES: usize = 30;
const MAX_QUALITY_BUTTONS: usize = 90;

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
    /// Titles merged from all providers (same name + release year)
    groups: Vec<Group>,
    /// Details of the chosen title from every provider that has it
    members: Vec<(ProviderKind, MediaDetails)>,
    /// Details used for the card and the season / episode lists
    primary: Option<MediaDetails>,
    /// "Name (Year)" of the chosen title
    title: String,
    season: usize,
    episode: usize,
    targets: Vec<Target>,
    releases: Vec<Release>,
    /// For every release: index into `targets`
    rel_target: Vec<usize>,
    /// Releases whose links turned out to be dead (hidden from the list)
    dead: Vec<usize>,
}

/// One provider subject the streams were fetched from.
#[derive(Clone)]
struct Target {
    provider: ProviderKind,
    subject_id: String,
    siblings: Vec<String>,
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
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
I search <b>all providers</b> at once and show one combined list:\n\
1. Pick the title (name + year)\n2. Season / Episode (series)\n3. All audio languages\n4. Pick a quality / source (links from every provider)\n\n\
Then play it in Telegram, MX Player or VLC, or tap ⬇️ Download.\n\n\
/search &lt;name&gt; - search all providers\n/id - show your chat id";

// ----------------------------------------------------------------- Search

/// Providers the bot searches (the TUI list plus built-in Stremio addons).
const BOT_PROVIDERS: [ProviderKind; 6] = [
    ProviderKind::MovieBox,
    ProviderKind::FourKHdHub,
    ProviderKind::Dramachi,
    ProviderKind::BdixCircleFtp,
    ProviderKind::BdixDhakaFlix,
    ProviderKind::Addons,
];

/// Order used to pick the provider whose metadata (poster, description,
/// seasons) is shown on the title card.
const DETAIL_PRIORITY: [ProviderKind; 6] = [
    ProviderKind::Addons,
    ProviderKind::MovieBox,
    ProviderKind::FourKHdHub,
    ProviderKind::Dramachi,
    ProviderKind::BdixCircleFtp,
    ProviderKind::BdixDhakaFlix,
];

/// One title (same name + release year) found on one or more providers.
#[derive(Clone)]
struct Group {
    title: String,
    year: String,
    series: bool,
    poster: Option<String>,
    members: Vec<(ProviderKind, CatalogItem)>,
}

/// Lower-case title without bracketed text, punctuation or a leading "the".
fn norm_title(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            '&' => out.push_str(" and "),
            c if c.is_alphanumeric() => out.extend(c.to_lowercase()),
            _ => out.push(' '),
        }
    }
    let joined = out.split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.strip_prefix("the ") {
        Some(rest) => rest.to_string(),
        None => joined,
    }
}

/// Title for display: original case, bracketed text removed.
fn clean_title(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            _ => out.push(c),
        }
    }
    let t = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        s.trim().to_string()
    } else {
        t
    }
}

/// First 4-digit year (19xx / 20xx) found in the text, or "".
fn year_of(y: Option<&str>) -> String {
    let Some(y) = y else {
        return String::new();
    };
    let chars: Vec<char> = y.chars().collect();
    for w in chars.windows(4) {
        let digits = w.iter().all(|c| c.is_ascii_digit());
        let century = (w[0] == '1' && w[1] == '9') || (w[0] == '2' && w[1] == '0');
        if digits && century {
            return w.iter().collect();
        }
    }
    String::new()
}

fn distinct_providers(members: &[(ProviderKind, CatalogItem)]) -> usize {
    let mut seen: Vec<ProviderKind> = Vec::new();
    for (p, _) in members {
        if !seen.contains(p) {
            seen.push(*p);
        }
    }
    seen.len()
}

/// Merge the results of all providers: same name + same release year = one title.
fn build_groups(results: &[(ProviderKind, Vec<CatalogItem>)]) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut keys: Vec<(String, String)> = Vec::new();
    // First the items that have a year, then the ones without (they join any
    // title with the same name).
    for with_year in [true, false] {
        for (p, items) in results {
            for it in items {
                let nt = norm_title(&it.title);
                let y = year_of(it.year.as_deref());
                if nt.is_empty() || y.is_empty() == with_year {
                    continue;
                }
                let series = it.media_type == MediaType::Series;
                let poster = it.poster_url.clone().filter(|u| u.starts_with("http"));
                let pos = keys
                    .iter()
                    .position(|(kt, ky)| *kt == nt && (y.is_empty() || *ky == y));
                match pos {
                    Some(i) => {
                        let g = &mut groups[i];
                        let dup = g
                            .members
                            .iter()
                            .any(|(mp, mi)| mp == p && mi.id.value == it.id.value);
                        if g.members.len() < 12 && !dup {
                            g.members.push((*p, it.clone()));
                        }
                        g.series |= series;
                        if g.poster.is_none() {
                            g.poster = poster;
                        }
                    }
                    None => {
                        keys.push((nt, y.clone()));
                        groups.push(Group {
                            title: clean_title(&it.title),
                            year: y,
                            series,
                            poster,
                            members: vec![(*p, it.clone())],
                        });
                    }
                }
            }
        }
    }
    // Titles that exist on more providers come first.
    groups.sort_by_key(|g| std::cmp::Reverse(distinct_providers(&g.members)));
    groups
}

async fn do_search(ctx: &Ctx, chat: i64, query: &str) {
    let wait = ctx
        .tg
        .send(
            chat,
            &format!("🔎 Searching all providers for <b>{}</b>…", esc(query)),
            None,
        )
        .await
        .ok();

    let futs = BOT_PROVIDERS.into_iter().map(|p| {
        let svc = ctx.svc.clone();
        let q = query.to_string();
        async move {
            let r = tokio::time::timeout(Duration::from_secs(25), svc.search_typed(p, &q, 1)).await;
            (p, r)
        }
    });
    let outs = futures::future::join_all(futs).await;

    let mut results: Vec<(ProviderKind, Vec<CatalogItem>)> = Vec::new();
    for (p, r) in outs {
        if let Ok(Ok(items)) = r {
            if !items.is_empty() {
                results.push((p, items));
            }
        }
    }
    let groups = build_groups(&results);
    let q = query.to_string();
    st_set(ctx, chat, |s| {
        *s = ChatState::default();
        s.query = q;
        s.groups = groups;
    });
    show_results(ctx, chat, wait, 0).await;
}

/// One combined list ("Total results") with every title from every provider.
async fn show_results(ctx: &Ctx, chat: i64, msg: Option<i64>, page: usize) {
    let st = st_get(ctx, chat);
    let total = st.groups.len();
    if total == 0 {
        let text = format!(
            "No results for <b>{}</b>. Try another name.",
            esc(&st.query)
        );
        show(ctx, chat, msg, &text, None).await;
        return;
    }
    let start = (page * PAGE_ITEMS).min(total);
    let end = (start + PAGE_ITEMS).min(total);
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for (gi, g) in st.groups.iter().enumerate().take(end).skip(start) {
        let icon = if g.series { "📺" } else { "🎬" };
        let year = if g.year.is_empty() {
            String::new()
        } else {
            format!(" ({})", g.year)
        };
        rows.push(vec![cb(
            &short(&format!("{icon} {}{year}", g.title), 56),
            format!("d:{gi}"),
        )]);
    }
    let mut nav = Vec::new();
    if page > 0 {
        nav.push(cb("◀ Prev", format!("g:{}", page - 1)));
    }
    if end < total {
        nav.push(cb("Next ▶", format!("g:{}", page + 1)));
    }
    if !nav.is_empty() {
        rows.push(nav);
    }
    let text = format!(
        "Results for <b>{}</b>\n\n✅ Total results: {}\nShowing {}-{}\n\nPick a title:",
        esc(&st.query),
        total,
        start + 1,
        end
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

fn card(d: &MediaDetails, providers: usize) -> String {
    let mut s = format!("<b>{}</b>", esc(&d.title));
    if let Some(y) = d.year.as_ref().filter(|y| !y.is_empty()) {
        s.push_str(&format!(" ({})", esc(y)));
    }
    s.push_str(&format!(
        "\n{} • {} provider{}",
        if d.is_series() { "Series" } else { "Movie" },
        providers,
        if providers == 1 { "" } else { "s" }
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

/// A title was picked: load its details from every provider that has it.
async fn open_group(ctx: &Ctx, chat: i64, gi: usize) {
    let st = st_get(ctx, chat);
    let Some(group) = st.groups.get(gi).cloned() else {
        expired(ctx, chat).await;
        return;
    };
    let wait = ctx
        .tg
        .send(chat, "⏳ Loading details from all providers…", None)
        .await
        .ok();

    let futs = group.members.iter().map(|(p, item)| {
        let svc = ctx.svc.clone();
        let p = *p;
        let id = item.id.value.clone();
        async move {
            let r = tokio::time::timeout(Duration::from_secs(30), svc.details_typed(p, &id)).await;
            (p, r)
        }
    });
    let outs = futures::future::join_all(futs).await;

    let mut members: Vec<(ProviderKind, MediaDetails)> = Vec::new();
    let mut first_err: Option<String> = None;
    for (p, r) in outs {
        match r {
            Ok(Ok(d)) => members.push((p, d)),
            Ok(Err(e)) => {
                if first_err.is_none() {
                    first_err = Some(e.user_message(p));
                }
            }
            Err(_) => {
                if first_err.is_none() {
                    first_err = Some(format!("{} timed out loading details.", p.label()));
                }
            }
        }
    }
    if members.is_empty() {
        let msg = first_err.unwrap_or_else(|| "Could not load details.".to_string());
        show(ctx, chat, wait, &format!("⚠️ {}", esc(&msg)), None).await;
        return;
    }
    members.sort_by_key(|(p, _)| {
        DETAIL_PRIORITY
            .iter()
            .position(|x| x == p)
            .unwrap_or(usize::MAX)
    });

    // Metadata / seasons come from the best provider that has them.
    let with_seasons = members.iter().find(|(_, d)| !d.seasons.is_empty());
    let chosen = if group.series {
        with_seasons.or(members.first())
    } else {
        members.first()
    };
    let Some(primary) = chosen.map(|(_, d)| d.clone()) else {
        expired(ctx, chat).await;
        return;
    };

    let mut seen: Vec<ProviderKind> = Vec::new();
    for (p, _) in &members {
        if !seen.contains(p) {
            seen.push(*p);
        }
    }
    let caption = card(&primary, seen.len());
    let ok_url = |u: &String| u.starts_with("http");
    let poster = primary
        .poster_url
        .clone()
        .filter(ok_url)
        .or_else(|| {
            members
                .iter()
                .find_map(|(_, d)| d.poster_url.clone().filter(ok_url))
        })
        .or_else(|| group.poster.clone());

    let year = if group.year.is_empty() {
        year_of(primary.year.as_deref())
    } else {
        group.year.clone()
    };
    let title = if year.is_empty() {
        group.title.clone()
    } else {
        format!("{} ({})", group.title, year)
    };
    let p2 = primary.clone();
    st_set(ctx, chat, |s| {
        s.members = members;
        s.primary = Some(p2);
        s.title = title;
        s.season = 0;
        s.episode = 0;
        s.targets.clear();
        s.releases.clear();
        s.rel_target.clear();
        s.dead.clear();
    });

    let mut sent_photo = false;
    if let Some(url) = poster {
        sent_photo = ctx.tg.photo(chat, &url, &caption).await.is_ok();
    }
    if !sent_photo {
        let _ = ctx.tg.send(chat, &caption, None).await;
    }
    if let Some(w) = wait {
        let _ = ctx
            .tg
            .call("deleteMessage", &json!({"chat_id": chat, "message_id": w}))
            .await;
    }

    if primary.is_series() && !primary.seasons.is_empty() {
        show_seasons(ctx, chat, None).await;
    } else {
        show_audio(ctx, chat, None).await;
    }
}

async fn show_seasons(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let st = st_get(ctx, chat);
    let Some(d) = st.primary.as_ref() else {
        expired(ctx, chat).await;
        return;
    };
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut row: Vec<Value> = Vec::new();
    for s in d.seasons.iter().take(40) {
        row.push(cb(
            &format!("Season {}", s.number),
            format!("s:{}:0", s.number),
        ));
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
    let Some(d) = st.primary.as_ref() else {
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

/// The only audio choice: everything from every provider.
async fn show_audio(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let rows = vec![vec![cb("🔊 All audio languages", "al".to_string())]];
    show(ctx, chat, msg, "Choose the audio language:", Some(kb(rows))).await;
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
            None => Err(ProviderError::Unavailable(
                "4KHDHub is unavailable".to_string(),
            )),
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
        ProviderKind::Addons => {
            ReleaseProvider::episode_streams(&svc.addon_client, id, season, episode).await
        }
    }
}

/// Fetch the streams of this title from every provider at the same time and
/// merge them into one list.
async fn fetch_all(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let st = st_get(ctx, chat);
    let target_msg = match msg {
        Some(m) => {
            let _ = ctx
                .tg
                .edit(
                    chat,
                    m,
                    "⏳ Fetching streams from all providers and checking the links…",
                    None,
                )
                .await;
            Some(m)
        }
        None => ctx
            .tg
            .send(
                chat,
                "⏳ Fetching streams from all providers and checking the links…",
                None,
            )
            .await
            .ok(),
    };

    // One target per provider subject (MovieBox has one subject per audio dub).
    let mut targets: Vec<Target> = Vec::new();
    for (p, d) in &st.members {
        let mut ids: Vec<String> = Vec::new();
        if *p == ProviderKind::MovieBox {
            ids.extend(d.dubs.iter().map(|x| x.subject_id.clone()));
        }
        ids.push(d.id.value.clone());
        let siblings = d.sibling_ids();
        for id in ids {
            if id.is_empty()
                || targets
                    .iter()
                    .any(|t| t.provider == *p && t.subject_id == id)
            {
                continue;
            }
            targets.push(Target {
                provider: *p,
                subject_id: id,
                siblings: siblings.clone(),
            });
        }
    }

    let futs = targets.iter().enumerate().map(|(ti, t)| {
        let svc = ctx.svc.clone();
        let p = t.provider;
        let id = t.subject_id.clone();
        let (season, episode) = (st.season, st.episode);
        async move {
            let r = tokio::time::timeout(
                Duration::from_secs(45),
                provider_streams(&svc, p, &id, season, episode),
            )
            .await;
            (ti, p, r)
        }
    });
    let outs = futures::future::join_all(futs).await;

    let mut tagged: Vec<(Release, usize)> = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    for (ti, p, r) in outs {
        match r {
            Ok(Ok(v)) => tagged.extend(v.into_iter().map(|x| (x, ti))),
            Ok(Err(ProviderError::NotFound)) => {}
            Ok(Err(e)) => errs.push(e.user_message(p)),
            Err(_) => errs.push(format!("{} timed out.", p.label())),
        }
    }
    // Hide dead links (404, error pages, ...). 4KHDHub pages are resolved and
    // checked later, when the quality is tapped.
    {
        use futures::StreamExt;
        let mut jobs: Vec<ProbeJob> = Vec::new();
        for (i, (rel, _)) in tagged.iter().enumerate() {
            if rel.provider == ProviderKind::FourKHdHub {
                continue;
            }
            for (mi, m) in rel.mirrors.iter().enumerate().take(2) {
                jobs.push((i, mi, m.resolver_url.clone(), m.headers.clone()));
            }
        }
        let checked: Vec<(usize, usize, bool)> = futures::stream::iter(jobs)
            .map(|(i, mi, url, headers)| async move {
                (i, mi, link_works(probe_client(), &url, &headers).await)
            })
            .buffer_unordered(40)
            .collect()
            .await;
        let dead: std::collections::HashSet<(usize, usize)> = checked
            .into_iter()
            .filter(|(_, _, ok)| !ok)
            .map(|(i, mi, _)| (i, mi))
            .collect();
        for (i, (rel, _)) in tagged.iter_mut().enumerate() {
            if rel.provider == ProviderKind::FourKHdHub {
                continue;
            }
            let mut mi = 0usize;
            rel.mirrors.retain(|_| {
                let keep = !dead.contains(&(i, mi));
                mi += 1;
                keep
            });
        }
        tagged
            .retain(|(rel, _)| rel.provider == ProviderKind::FourKHdHub || !rel.mirrors.is_empty());
    }
    if tagged.is_empty() {
        let mut text = "⚠️ No streams found for this item.".to_string();
        errs.dedup();
        for e in errs.iter().take(3) {
            text.push_str(&format!("\n• {}", esc(&short(e, 120))));
        }
        show(ctx, chat, target_msg, &text, None).await;
        return;
    }
    tagged.sort_by(|a, b| {
        b.0.resolution_u64()
            .cmp(&a.0.resolution_u64())
            .then_with(|| b.0.size_bytes.cmp(&a.0.size_bytes))
    });
    let (releases, rel_target): (Vec<Release>, Vec<usize>) = tagged.into_iter().unzip();
    st_set(ctx, chat, |s| {
        s.targets = targets;
        s.releases = releases;
        s.rel_target = rel_target;
        s.dead.clear();
    });
    show_qualities(ctx, chat, target_msg).await;
}

async fn show_qualities(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let st = st_get(ctx, chat);
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let live: Vec<(usize, &Release)> = st
        .releases
        .iter()
        .enumerate()
        .filter(|(i, _)| !st.dead.contains(i))
        .collect();
    for (i, r) in live.iter().take(MAX_QUALITY_BUTTONS) {
        rows.push(vec![cb(&rel_label(r), format!("r:{i}"))]);
    }
    let text = format!(
        "<b>{}</b>{}\nChoose quality / source ({} available):",
        esc(&st.title),
        esc(&episode_tag(&st)),
        live.len()
    );
    show(ctx, chat, msg, &text, Some(kb(rows))).await;
}

// ------------------------------------------------------------ Link checking

/// (release index, mirror index, url, headers)
type ProbeJob = (usize, usize, String, Vec<(String, String)>);

fn probe_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(6))
            .user_agent("Mozilla/5.0 (Linux; Android 13) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Mobile Safari/537.36")
            .build()
            .unwrap_or_default()
    })
}

fn host_of(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
}

/// A link is only shown when it passes both tests.
const MIN_LINK_BYTES: u64 = 50 * 1024 * 1024;

/// Two tests before a link is sent to the user:
/// 1. the link answers HTTP 200 (206 when the host honours our Range request)
/// 2. the file is bigger than 50 MB
///
/// Err(reason) means the link is hidden.
async fn link_status(
    http: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<(), String> {
    let mut req = http.get(url).header("Range", "bytes=0-1");
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = match tokio::time::timeout(Duration::from_secs(10), req.send()).await {
        Err(_) => return Err("no answer (timed out)".to_string()),
        Ok(Err(_)) => return Err("cannot connect".to_string()),
        Ok(Ok(r)) => r,
    };

    // Test 1: HTTP 200
    let status = resp.status().as_u16();
    if status != 200 && status != 206 {
        return Err(format!("HTTP {status}"));
    }

    // Test 2: size greater than 50 MB
    let header_num = |name: &str| -> Option<u64> {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    let total = if status == 206 {
        resp.headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.trim().parse::<u64>().ok())
    } else {
        header_num("content-length")
    };
    match total {
        Some(t) if t > MIN_LINK_BYTES => Ok(()),
        Some(t) => Err(format!("file is only {} MB", t / (1024 * 1024))),
        None => Err("file size unknown".to_string()),
    }
}

async fn link_works(http: &reqwest::Client, url: &str, headers: &[(String, String)]) -> bool {
    link_status(http, url, headers).await.is_ok()
}

// ------------------------------------------------------------------ Links

/// (label, url, headers)
type LinkEntry = (String, String, Vec<(String, String)>);

async fn send_links(ctx: &Ctx, chat: i64, k: usize) {
    let st = st_get(ctx, chat);
    let Some(rel) = st.releases.get(k).cloned() else {
        expired(ctx, chat).await;
        return;
    };
    let wait = ctx.tg.send(chat, "⏳ Getting links…", None).await.ok();

    // (label, url, headers)
    let mut links: Vec<LinkEntry> = Vec::new();
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
            &format!(
                "⚠️ {}",
                esc(&problem.unwrap_or_else(|| "No links.".to_string()))
            ),
            None,
        )
        .await;
        return;
    }

    // Drop links that are dead so Play / Download never get them.
    let checks = futures::future::join_all(
        links
            .iter()
            .map(|(_, url, headers)| link_status(probe_client(), url, headers)),
    )
    .await;
    let mut reason: Option<String> = None;
    let mut alive: Vec<LinkEntry> = Vec::new();
    for (l, c) in links.into_iter().zip(checks) {
        match c {
            Ok(()) => alive.push(l),
            Err(e) => {
                if reason.is_none() {
                    reason = Some(format!("{e} on {}", host_of(&l.1)));
                }
            }
        }
    }
    let links = alive;
    if links.is_empty() {
        st_set(ctx, chat, |s| {
            if !s.dead.contains(&k) {
                s.dead.push(k);
            }
        });
        let rows = vec![vec![cb("⬅ Other qualities", "q".to_string())]];
        let why = reason.unwrap_or_else(|| "no working link".to_string());
        show(
            ctx,
            chat,
            wait,
            &format!(
                "⚠️ This link is not working ({}), so I removed it from the list. Please pick another quality.",
                esc(&why)
            ),
            Some(kb(rows)),
        )
        .await;
        return;
    }

    let title = st.title.clone();
    let tgt = st
        .rel_target
        .get(k)
        .and_then(|ti| st.targets.get(*ti))
        .cloned();
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
    if rel.provider == ProviderKind::MovieBox {
        if let (Some(rid), Some(t)) = (rel.resource_id.as_ref(), tgt.as_ref()) {
            let caps = tokio::time::timeout(
                Duration::from_secs(15),
                ctx.svc
                    .get_ext_captions(&t.subject_id, rid, &t.siblings, st.season, st.episode),
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
    let locked_text =
        format!("{base_text}\n\n🔒 <b>Watch 2 short ads to unlock Play and Download.</b>");
    insert_stream(
        &ctx.streams,
        &sid,
        Stream {
            items,
            title: format!("{}{}", title, episode_tag(&st)),
            file_base: format!(
                "{}{} {}",
                title,
                episode_tag(&st),
                rel.quality.clone().unwrap_or_default()
            ),
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
    let text = msg
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return;
    }
    if text.starts_with("/id") {
        let _ = ctx
            .tg
            .send(chat, &format!("Chat id: <code>{chat}</code>"), None)
            .await;
        return;
    }
    if !authorised(&ctx, chat) {
        let _ = ctx
            .tg
            .send(chat, "⛔ You are not allowed to use this bot.", None)
            .await;
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
                    let _ = ctx
                        .tg
                        .send(chat, "Usage: /search &lt;movie or series name&gt;", None)
                        .await;
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
    let id = cq
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    ctx.tg.answer(&id, "").await;
    let Some(chat) = cq.pointer("/message/chat/id").and_then(Value::as_i64) else {
        return;
    };
    if !authorised(&ctx, chat) {
        return;
    }
    let msg_id = cq.pointer("/message/message_id").and_then(Value::as_i64);
    let data = cq
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let parts: Vec<&str> = data.split(':').collect();
    let num = |i: usize| parts.get(i).and_then(|s| s.parse::<usize>().ok());

    match parts.first().copied().unwrap_or("") {
        "g" => {
            if let Some(page) = num(1) {
                show_results(&ctx, chat, msg_id, page).await;
            }
        }
        "d" => {
            if let Some(gi) = num(1) {
                open_group(&ctx, chat, gi).await;
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
                    s.targets.clear();
                    s.releases.clear();
                    s.rel_target.clear();
                    s.dead.clear();
                });
                show_audio(&ctx, chat, msg_id).await;
            }
        }
        "al" => fetch_all(&ctx, chat, msg_id).await,
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
    /// base name of the downloaded file (title + episode + quality)
    file_base: String,
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

/// https://host/p/ID  ->  https://host/d/ID?n=N  (file download link)
fn dl_url(page: &str, n: usize) -> String {
    match page.rfind("/p/") {
        Some(i) => format!("{}/d/{}?n={}", &page[..i], &page[i + 3..], n),
        None => format!("{page}?n={n}"),
    }
}

/// File extension for a download: from the link, else from the content type.
fn file_ext(url: &str, ctype: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let ext = last
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    for known in ["mkv", "mp4", "avi", "webm", "mov", "m4v", "ts"] {
        if ext == known {
            return known;
        }
    }
    let ct = ctype.to_ascii_lowercase();
    if ct.contains("matroska") {
        "mkv"
    } else if ct.contains("webm") {
        "webm"
    } else {
        "mp4"
    }
}

/// Safe ASCII file name for the Content-Disposition header.
fn download_name(base: &str, ext: &str) -> String {
    let mut name = String::new();
    for c in base.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')') {
            name.push(c);
        } else {
            name.push('_');
        }
    }
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = name.trim_matches(|c| c == '_' || c == ' ' || c == '.');
    let name = if name.is_empty() { "video" } else { name };
    format!("{name}.{ext}")
}

/// Play buttons that appear after the 2 ads were watched.
fn play_keyboard(st: &Stream, webapp: bool) -> Value {
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for (i, it) in st.items.iter().enumerate() {
        let page = format!("{}?n={}", st.page, i);
        let label = format!("▶️ Play: {}", short(&it.name, 30));
        if webapp {
            rows.push(vec![
                json!({"text": label, "web_app": {"url": page.clone()}}),
            ]);
        } else {
            rows.push(vec![json!({"text": label, "url": page.clone()})]);
        }
        rows.push(vec![
            json!({"text": "📱 MX Player", "url": format!("{page}&go=mx")}),
            json!({"text": "🎞 VLC", "url": format!("{page}&go=vlc")}),
        ]);
        rows.push(vec![
            json!({"text": "⬇️ Download", "url": dl_url(&st.page, i)}),
        ]);
    }
    rows.push(vec![cb("⬅ Other qualities", "q".to_string())]);
    kb(rows)
}

/// How often the countdown in the message is refreshed (Telegram limits how
/// fast a bot may edit a message, so it ticks every few seconds).
const COUNTDOWN_TICK_SECS: u64 = 5;

/// 7325 -> "02:02:05" (hours:minutes:seconds)
fn fmt_hms(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn unlocked_text(text: &str, secs_left: u64) -> String {
    format!(
        "{text}\n\n✅ <b>Unlocked.</b> Tap Play or Download below.\n⏱ Time left: <b>{}</b> (hr:min:sec)",
        fmt_hms(secs_left)
    )
}

/// After unlocking: count down in the message (hr:min:sec), and when the time
/// is over remove the Play buttons and the stream.
#[allow(clippy::too_many_arguments)]
async fn countdown_task(
    tg: Tg,
    streams: StreamMap,
    id: String,
    chat: i64,
    msg: i64,
    text: String,
    keyboard: Value,
    deadline: Instant,
) {
    let mut editing = true;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let left = deadline - now;
        if !editing {
            // Message was deleted: just wait for the end.
            tokio::time::sleep(left).await;
            break;
        }
        tokio::time::sleep(Duration::from_secs(COUNTDOWN_TICK_SECS).min(left)).await;
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let secs_left = (deadline - now).as_secs();
        let t = unlocked_text(&text, secs_left);
        if let Err(e) = tg.edit(chat, msg, &t, Some(keyboard.clone())).await {
            let e = e.to_ascii_lowercase();
            if e.contains("not found")
                || e.contains("message_id_invalid")
                || e.contains("can't be edited")
            {
                editing = false;
            } else if e.contains("too many requests") || e.contains("retry after") {
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        }
    }
    if let Ok(mut g) = streams.lock() {
        g.remove(&id);
    }
    let t = format!("{text}\n\n⌛ <b>Expired.</b> Tap a quality again and watch 2 ads to play.");
    let _ = tg
        .edit(
            chat,
            msg,
            &t,
            Some(kb(vec![vec![cb("⬅ Other qualities", "q".to_string())]])),
        )
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
a.d{background:#198754}
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
<div class="row"><a class="b" id="mx" href="#">MX Player</a><a class="b v" id="vlc" href="#">VLC</a><a class="b d" id="dl" href="#">Download</a></div>
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
document.getElementById("dl").onclick=function(e){e.preventDefault();var u=location.origin+"/d/__ID__?n=__N__";if(inTg&&tg.openLink){tg.openLink(u);}else{location.href=u;}};
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
<p>Watch 2 short ads to unlock the video and the download. It stays unlocked for 6 hours.</p>
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
    GATE_HTML
        .replace("__TITLE__", &esc(title))
        .replace("__ID__", id)
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
                let deadline =
                    st.unlocked_at.unwrap_or_else(Instant::now) + Duration::from_secs(UNLOCK_SECS);
                let text = unlocked_text(&st.text, UNLOCK_SECS);
                let mut keyboard = play_keyboard(&st, true);
                if tg
                    .edit(st.chat, st.msg, &text, Some(keyboard.clone()))
                    .await
                    .is_err()
                {
                    keyboard = play_keyboard(&st, false);
                    let _ = tg
                        .edit(st.chat, st.msg, &text, Some(keyboard.clone()))
                        .await;
                }
                tokio::spawn(countdown_task(
                    tg.clone(),
                    streams.clone(),
                    id.to_string(),
                    st.chat,
                    st.msg,
                    st.text.clone(),
                    keyboard,
                    deadline,
                ));
            }
            http_simple(&mut sock, 200, "OK", "application/json", b"{\"ok\":true}").await;
            return;
        }
    }

    if let Some(id) = path.strip_prefix("/p/") {
        let Some(entry) = get(id) else {
            http_simple(
                &mut sock,
                404,
                "Not Found",
                "text/plain",
                b"Expired. Tap the quality again in the bot.",
            )
            .await;
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
        http_simple(
            &mut sock,
            200,
            "OK",
            "text/html; charset=utf-8",
            html.as_bytes(),
        )
        .await;
        return;
    }

    let route = path
        .strip_prefix("/s/")
        .map(|id| (id, false))
        .or_else(|| path.strip_prefix("/d/").map(|id| (id, true)));
    if let Some((id, download)) = route {
        let Some(entry) = get(id) else {
            http_simple(&mut sock, 404, "Not Found", "text/plain", b"Expired").await;
            return;
        };
        if !entry.is_unlocked() {
            http_simple(
                &mut sock,
                403,
                "Forbidden",
                "text/plain",
                b"Locked: watch the ads first",
            )
            .await;
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
            Err(e) => {
                eprintln!("[proxy] upstream error for {}: {e}", host_of(&item.url));
                http_simple(
                    &mut sock,
                    502,
                    "Bad Gateway",
                    "text/plain",
                    b"upstream error",
                )
                .await;
                return;
            }
        };
        let status = resp.status();
        if status.as_u16() >= 400 {
            eprintln!(
                "[proxy] upstream HTTP {} for {}",
                status.as_u16(),
                host_of(&item.url)
            );
        }
        let mut out = format!(
            "HTTP/1.1 {} {}\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or("OK")
        );
        let mut has_ct = false;
        let mut ctype = String::new();
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
                    ctype = v.to_string();
                }
                out.push_str(&format!("{name}: {v}\r\n"));
            }
        }
        if !has_ct {
            out.push_str("content-type: video/mp4\r\n");
        }
        if download {
            let fname = download_name(&entry.file_base, file_ext(&item.url, &ctype));
            out.push_str(&format!(
                "content-disposition: attachment; filename=\"{fname}\"\r\n"
            ));
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

    http_simple(
        &mut sock,
        200,
        "OK",
        "text/plain",
        b"MahSurat Stream Bot player server",
    )
    .await;
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
        let name = if cfg!(windows) {
            "cloudflared.exe"
        } else {
            "cloudflared"
        };
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
                println!(
                    "moviebox-tgbot [--token BOT_TOKEN] [--allow CHAT_ID,CHAT_ID]\n\nThe token and allowed ids are saved, so next time just run: moviebox-tgbot"
                );
                return;
            }
            _ => {}
        }
        i += 1;
    }

    let env_token = std::env::var("BOT_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
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
        .or_else(|| {
            std::env::var("ALLOWED_CHAT_IDS")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
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

    println!(
        "Bot started ({})",
        if on_github { "GitHub Actions" } else { "local" }
    );
    let deadline: Option<Instant> =
        run_minutes.map(|m| Instant::now() + Duration::from_secs(m * 60));
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

#[cfg(test)]
mod tests {
    use super::*;
    use moviebox_tui::providers::models::ProviderMediaId;

    fn item(p: ProviderKind, id: &str, title: &str, year: Option<&str>) -> CatalogItem {
        CatalogItem {
            id: ProviderMediaId {
                provider: p,
                value: id.to_string(),
            },
            title: title.to_string(),
            media_type: MediaType::Movie,
            year: year.map(str::to_string),
            poster_url: None,
            season_count: None,
        }
    }

    #[test]
    fn same_name_and_year_merge_across_providers() {
        let results = vec![
            (
                ProviderKind::MovieBox,
                vec![
                    item(ProviderKind::MovieBox, "m1", "Alpha", Some("2025")),
                    item(ProviderKind::MovieBox, "m2", "Alpha", Some("2013")),
                ],
            ),
            (
                ProviderKind::Dramachi,
                vec![item(
                    ProviderKind::Dramachi,
                    "d1",
                    "Alpha (2025)",
                    Some("2025-03-01"),
                )],
            ),
            (
                ProviderKind::Addons,
                vec![item(
                    ProviderKind::Addons,
                    "movie:tt1",
                    "The Alpha",
                    Some("2025"),
                )],
            ),
        ];
        let groups = build_groups(&results);
        assert_eq!(groups.len(), 2, "Alpha 2025 and Alpha 2013 stay separate");
        // the title found on the most providers comes first
        assert_eq!(groups[0].year, "2025");
        assert_eq!(distinct_providers(&groups[0].members), 3);
        assert_eq!(groups[1].year, "2013");
        assert_eq!(groups[1].members.len(), 1);
    }

    #[test]
    fn titles_and_years_are_normalised() {
        assert_eq!(norm_title("The Avengers [Hindi]"), "avengers");
        assert_eq!(norm_title("Tom & Jerry"), "tom and jerry");
        assert_eq!(year_of(Some("Released 2012-05-04")), "2012");
        assert_eq!(year_of(Some("n/a")), "");
        assert_eq!(clean_title("Alpha [Hindi] (2025)"), "Alpha");
    }

    #[test]
    fn download_link_and_file_name() {
        assert_eq!(
            dl_url("https://x.trycloudflare.com/p/abc123", 2),
            "https://x.trycloudflare.com/d/abc123?n=2"
        );
        assert_eq!(
            file_ext("https://h/a/movie.mkv?token=1", "video/mp4"),
            "mkv"
        );
        assert_eq!(file_ext("https://h/stream", "video/x-matroska"), "mkv");
        assert_eq!(file_ext("https://h/stream", ""), "mp4");
        assert_eq!(
            download_name("Alpha (2025) 2160p", "mkv"),
            "Alpha (2025) 2160p.mkv"
        );
        assert_eq!(download_name("___", "mp4"), "video.mp4");
    }

    async fn serve_once(
        status_line: &'static str,
        headers: &'static str,
        body: &'static [u8],
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = format!("HTTP/1.1 {status_line}\r\n{headers}connection: close\r\n\r\n");
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}/file.mkv")
    }

    #[tokio::test]
    async fn links_need_http_200_and_more_than_50mb() {
        let http = reqwest::Client::new();

        // passes: 206 and a 5 GB file
        let big = serve_once(
            "206 Partial Content",
            "content-type: video/x-matroska\r\ncontent-range: bytes 0-1/5000000000\r\ncontent-length: 2\r\n",
            b"ab",
        )
        .await;
        assert!(link_status(&http, &big, &[]).await.is_ok());

        // passes: host ignores Range, answers 200 with a 700 MB length
        let full = serve_once(
            "200 OK",
            "content-type: video/mp4\r\ncontent-length: 734003200\r\n",
            b"x",
        )
        .await;
        assert!(link_status(&http, &full, &[]).await.is_ok());

        // fails test 1: not found
        let missing = serve_once(
            "404 Not Found",
            "content-type: text/plain\r\ncontent-length: 0\r\n",
            b"",
        )
        .await;
        assert_eq!(
            link_status(&http, &missing, &[]).await,
            Err("HTTP 404".to_string())
        );

        // fails test 2: only 20 MB
        let small = serve_once(
            "206 Partial Content",
            "content-range: bytes 0-1/20971520\r\ncontent-length: 2\r\n",
            b"ab",
        )
        .await;
        assert!(link_status(&http, &small, &[]).await.is_err());

        // fails test 2: size unknown
        let unknown = serve_once("200 OK", "content-type: video/mp4\r\n", b"x").await;
        assert!(link_status(&http, &unknown, &[]).await.is_err());
    }

    #[test]
    fn countdown_is_shown_as_hours_minutes_seconds() {
        assert_eq!(fmt_hms(UNLOCK_SECS), "06:00:00");
        assert_eq!(fmt_hms(7325), "02:02:05");
        assert_eq!(fmt_hms(59), "00:00:59");
        assert_eq!(fmt_hms(0), "00:00:00");
        assert!(unlocked_text("x", 3661).contains("01:01:01"));
    }
}
