# Web Search

## Description
Multi-source web research with citations. Use this skill when the user asks
about current events, factual queries, recent news, or wants deeper context
than the model's training data provides.

## Tools
- name: web_search
  kind: builtin
- name: fetch_url
  kind: builtin

## Instructions
- Issue multiple search queries when breadth matters; reformulate if the first
  pass returns thin or off-topic results.
- Always cite sources with URLs. Prefer permalinks over homepage links.
- Distinguish between primary sources (organizations, official releases,
  papers) and secondary sources (news articles, blog posts).
- When facts conflict between sources, surface the conflict explicitly to the
  user rather than silently picking one.
- Do not fabricate URLs or quote content you have not actually fetched.
- For time-sensitive questions, include the publication date next to each
  citation.
