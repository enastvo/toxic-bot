-- Tool-use settings (Tier 1 local tools + whitelist-restricted web search).
-- All default OFF so the feature is inert until the operator enables it.
ALTER TABLE settings ADD COLUMN tools_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE settings ADD COLUMN web_search_enabled INTEGER NOT NULL DEFAULT 0;
-- Comma/newline/space separated list of allowed domains for web search.
ALTER TABLE settings ADD COLUMN search_whitelist TEXT NOT NULL DEFAULT 'wikipedia.org,wikidata.org,wiktionary.org,britannica.com,merriam-webster.com,nasa.gov,noaa.gov,weather.gov,nih.gov,ncbi.nlm.nih.gov,cdc.gov,nist.gov,who.int,arxiv.org,nature.com,science.org,reuters.com,apnews.com,bbc.com,npr.org,pbs.org,theguardian.com,economist.com,developer.mozilla.org,docs.python.org,docs.rs,stackoverflow.com,github.com,man7.org';
-- Hard cap on tool-call rounds per turn (bounds cost: each round is another generation).
ALTER TABLE settings ADD COLUMN max_tool_rounds INTEGER NOT NULL DEFAULT 2;
