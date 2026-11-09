/**
 * Moviebox provider scraper stub (best-effort).
 * Mirrors the structure of the MovieBox TUI sources found in the project.
 */

const axios = require('axios');
const cheerio = require('cheerio');
const { BaseProvider, StreamItem, Stream } = require('./base');

const BASE_URL = 'https://moviebox.example';

class MovieboxProvider extends BaseProvider {
  constructor() {
    super('moviebox');
  }

  async search(query) {
    const { data } = await axios.get(`${BASE_URL}/search`, {
      params: { q: query },
      timeout: 12000,
      headers: { 'User-Agent': 'Mozilla/5.0 MahSuratBot/1.0' },
    });
    const $ = cheerio.load(data);
    const items = [];

    $('.card, .movie-item, .result').each((_, el) => {
      const $el = $(el);
      const $a = $el.find('a').first();
      const href = $a.attr('href') || '';
      const title = $a.attr('title') || $a.text().trim() || $el.find('h2, h3').text().trim();
      if (!title || !href) return;
      const id = href.split('/').filter(Boolean).pop() || href;
      const thumbnail = $el.find('img').attr('src') || null;
      items.push(new StreamItem({
        provider: this.name,
        id,
        title,
        thumbnail: thumbnail ? new URL(thumbnail, BASE_URL).href : null,
        description: $el.text().trim().slice(0, 200),
        video_url: new URL(href, BASE_URL).href,
      }));
    });

    return items.slice(0, 20);
  }

  async getStream(id) {
    const url = id.startsWith('http') ? id : new URL(id, BASE_URL).href;
    const { data } = await axios.get(url, {
      timeout: 12000,
      headers: { 'User-Agent': 'Mozilla/5.0 MahSuratBot/1.0' },
    });
    const $ = cheerio.load(data);
    const title = $('h1').first().text().trim() || id;
    let video_url = $('video source').attr('src') || null;
    if (!video_url) {
      // video_url = (data.match(/(https?:\/\/[^"'\s]+\.(mp4|m3u8)[^"'\s]*)/i) || [])[1] || null;
      video_url = null;
    }
    const thumbnail = $('meta[property="og:image"]').attr('content') || null;
    return new Stream({
      provider: this.name,
      id,
      title,
      video_url,
      thumbnail: thumbnail ? new URL(thumbnail, BASE_URL).href : null,
      description: $('meta[property="og:description"]').attr('content') || '',
      quality: 'HD',
    });
  }
}

module.exports = MovieboxProvider;
