/**
 * Mock provider with built-in sample content.
 * Guarantees the bot returns results even when real scrapers fail or
 * no network is available. Replace with real scrapers as needed.
 */

const { BaseProvider, StreamItem, Stream } = require('./base');

const CATALOG = [
  { id: 'm1', title: 'Big Buck Bunny', year: 2008, type: 'movie', description: 'A giant bunny goes on a rampage.', video_url: 'https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4', thumbnail: 'https://peach.blender.org/wp-content/uploads/title_08.jpg?1620173088' },
  { id: 'm2', title: 'Elephants Dream', year: 2006, type: 'movie', description: "The world's first CG animated short film.", video_url: 'https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/ElephantsDream.mp4', thumbnail: 'https://upload.wikimedia.org/wikipedia/commons/thumb/c/c5/Elephants_dream_s5_proog.jpg/320px-Elephants_dream_s5_proog.jpg' },
  { id: 'm3', title: 'Sintel', year: 2010, type: 'movie', description: 'A girl searches for her baby dragon.', video_url: 'https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/Sintel.mp4', thumbnail: 'https://durian.blender.org/wp-content/uploads/2010/12/sintel-poster.jpg' },
  { id: 's1', title: 'Sample Series — Episode 1', year: 2023, type: 'series', description: 'Pilot episode of the sample series.', video_url: 'https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/ForBiggerBlazes.mp4', thumbnail: 'https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/images/ForBiggerBlazes.jpg' },
  { id: 's2', title: 'Sample Series — Episode 2', year: 2023, type: 'series', description: 'The chase continues.', video_url: 'https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/ForBiggerEscapes.mp4', thumbnail: 'https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/images/ForBiggerEscapes.jpg' },
];

class MockProvider extends BaseProvider {
  constructor() {
    super('mock');
  }

  async search(query) {
    const q = (query || '').toLowerCase();
    return CATALOG.filter(item => !q || item.title.toLowerCase().includes(q) || (item.description || '').toLowerCase().includes(q)).map(item => new StreamItem({
      provider: this.name,
      id: item.id,
      title: item.title,
      year: item.year,
      type: item.type,
      thumbnail: item.thumbnail,
      description: item.description,
      video_url: item.video_url,
    }));
  }

  async getStream(id) {
    const item = CATALOG.find(i => i.id === id);
    if (!item) return null;
    return new Stream({
      provider: this.name,
      id: item.id,
      title: item.title,
      video_url: item.video_url,
      thumbnail: item.thumbnail,
      description: item.description,
      quality: 'HD',
    });
  }
}

module.exports = MockProvider;
