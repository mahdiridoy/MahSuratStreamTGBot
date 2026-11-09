/**
 * Provider registry.
 * Aggregates search results from ALL providers and deduplicates by title.
 */

const BaseProvider = require('./base').BaseProvider;
const MockProvider = require('./mock');

let DramachiProvider;
let BdixProvider;
let MovieboxProvider;

try { DramachiProvider = require('./dramachi'); } catch (e) { /* scraper stub missing */ }
try { BdixProvider = require('./bdix'); } catch (e) { /* scraper stub missing */ }
try { MovieboxProvider = require('./moviebox'); } catch (e) { /* scraper stub missing */ }

const providers = [
  new MockProvider(),
  DramachiProvider ? new DramachiProvider() : null,
  BdixProvider ? new BdixProvider() : null,
  MovieboxProvider ? new MovieboxProvider() : null,
].filter(Boolean);

/**
 * Search every provider and merge results.
 * Falls back gracefully if a single provider errors out.
 */
async function searchAllProviders(query) {
  if (!query || !query.trim()) return [];

  const searches = providers.map(p =>
    p.search(query.trim()).catch(err => {
      console.warn(`[provider:${p.name}] search failed:`, err.message);
      return [];
    })
  );

  const lists = await Promise.all(searches);
  const merged = lists.flat();

  // Deduplicate by provider:title so the same movie from two providers shows once per provider
  const seen = new Set();
  const unique = [];
  for (const item of merged) {
    const key = `${item.provider}:${item.title}`;
    if (seen.has(key)) continue;
    seen.add(key);
    unique.push(item);
  }

  return unique.slice(0, 100);
}

function getProviderByName(name) {
  return providers.find(p => p.name === name) || null;
}

function listProviders() {
  return providers.map(p => p.name);
}

module.exports = { searchAllProviders, getProviderByName, listProviders, providers };
