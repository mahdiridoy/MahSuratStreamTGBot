/**
 * Dashboard frontend: load favorites, add/remove, GitHub logout.
 */

const API = '/api';
const userName = document.getElementById('userName');
const userLogin = document.getElementById('userLogin');
const avatar = document.getElementById('avatar');
const favList = document.getElementById('favList');
const empty = document.getElementById('empty');
const addBtn = document.getElementById('addBtn');
const addTitle = document.getElementById('addTitle');
const logoutBtn = document.getElementById('logoutBtn');

async function loadMe() {
  const res = await fetch(`${API}/me`);
  const user = await res.json();
  if (!user) { location.href = '/'; return; }
  userName.textContent = user.login || 'GitHub user';
  userLogin.textContent = `GitHub id ${user.githubId}`;
  avatar.textContent = (user.login || 'U').slice(0, 1).toUpperCase();
  loadFavorites();
}

async function loadFavorites() {
  const res = await fetch(`${API}/favorites`);
  const favs = await res.json();
  render(favs);
}

function render(favs) {
  favList.innerHTML = '';
  empty.style.display = favs.length ? 'none' : 'block';
  for (const f of favs) {
    const li = document.createElement('li');
    li.innerHTML = `
      <div>
        <div class="title">${escapeHtml(f.title)}</div>
        <div class="meta">${escapeHtml(f.provider)} ${f.year ? '&bull; ' + f.year : ''} ${f.type ? '&bull; ' + f.type : ''}</div>
      </div>
      <button class="remove" data-provider="${escapeHtml(f.provider)}" data-id="${escapeHtml(f.id)}">Remove</button>
    `;
    favList.appendChild(li);
  }
  favList.querySelectorAll('.remove').forEach(btn => {
    btn.addEventListener('click', () => {
      removeFavorite(btn.dataset.provider, btn.dataset.id);
    });
  });
}

async function addFavoriteManual() {
  const title = addTitle.value.trim();
  if (!title) return;
  addBtn.disabled = true;
  try {
    await fetch(`${API}/favorites`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ provider: 'manual', id: 'm-' + Date.now(), title, year: null, type: 'movie', video_url: '', thumbnail: '' }),
    });
    addTitle.value = '';
    loadFavorites();
  } finally {
    addBtn.disabled = false;
  }
}

async function removeFavorite(provider, id) {
  await fetch(`${API}/favorites/${encodeURIComponent(provider)}/${encodeURIComponent(id)}`, { method: 'DELETE' });
  loadFavorites();
}

addBtn.addEventListener('click', addFavoriteManual);
addTitle.addEventListener('keydown', e => { if (e.key === 'Enter') addFavoriteManual(); });
logoutBtn.addEventListener('click', () => { fetch('/logout', { method: 'GET' }).then(() => { location.href = '/'; }); });

loadMe();

function escapeHtml(s) {
  return String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}
