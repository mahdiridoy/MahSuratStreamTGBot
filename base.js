/**
 * Base provider class.
 * Every provider implements search(query) -> StreamItem[] and getStream(id) -> Stream.
 * Providers must never throw: on failure they return [] / null so one broken
 * provider never kills the whole search.
 */

class StreamItem {
  constructor({ provider, id, title, year, type, thumbnail, description, video_url }) {
    this.provider = provider;
    this.id = id;
    this.title = title;
    this.year = year || null;
    this.type = type || 'movie'; // movie | series | episode
    this.thumbnail = thumbnail || null;
    this.description = description || '';
    this.video_url = video_url || null; // direct stream URL (used for play)
  }
}

class Stream {
  constructor({ provider, id, title, video_url, thumbnail, description, quality }) {
    this.provider = provider;
    this.id = id;
    this.title = title;
    this.video_url = video_url;
    this.thumbnail = thumbnail || null;
    this.description = description || '';
    this.quality = quality || null;
  }
}

class BaseProvider {
  constructor(name) {
    this.name = name;
  }

  async search(query) {
    throw new Error(`search() not implemented for ${this.name}`);
  }

  async getStream(id) {
    throw new Error(`getStream() not implemented for ${this.name}`);
  }
}

module.exports = { BaseProvider, StreamItem, Stream };
