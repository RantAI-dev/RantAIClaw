# Summarizer

## Description
Long-document and meeting summarization with structured output. Use this
skill when the user pastes or links to a long document, transcript, or set
of articles and wants the key points distilled.

## Tools
- name: fetch_url
  kind: builtin
- name: file_read
  kind: builtin

## Instructions
- Always ask (or infer from context) the desired length: bullets, one
  paragraph, or executive summary.
- Lead with a one-sentence TL;DR, then the structured body.
- Preserve named entities (people, organizations, dates, dollar amounts)
  verbatim — never paraphrase numbers.
- For multi-source summaries, attribute claims to their source inline.
- Surface disagreements between sources rather than silently picking one.
- If the document is too long for the context window, chunk it and summarize
  hierarchically (section summaries → final summary).
- Do not invent action items or decisions that weren't in the source.
