# Knowledge Index

This directory hosts a small, local knowledge index to help AI and humans query the SlopOS codebase by semantic similarity.

## Setup

```sh
python3 -m venv knowledge/.venv
. knowledge/.venv/bin/activate
pip install -r knowledge/requirements.txt
```

## Build or refresh the index

```sh
python knowledge/index.py
```

## Ask questions

```sh
python knowledge/query.py "where is the IOAPIC discovered?"
```

## Notes

- The index database and embeddings are local artifacts; do not commit them.
- Rebuild the index after large refactors or when new subsystems land.
- Only Rust (`.rs`) and Markdown (`.md`) files are indexed.
- By default this uses the `sentence-transformers/all-MiniLM-L6-v2` model.
  The first run will download the model if it is not cached.
