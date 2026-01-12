#!/usr/bin/env python3
import argparse
import json
import sys
from pathlib import Path

import faiss
import numpy as np
from sentence_transformers import SentenceTransformer

DEFAULT_MODEL = "sentence-transformers/all-MiniLM-L6-v2"
DEFAULT_INDEX_DIR = Path.cwd() / "index_data"  # Where the script was run from


def main() -> int:
    parser = argparse.ArgumentParser(description="Query the semantic search index.")
    parser.add_argument("query", help="Natural language question")
    parser.add_argument("--index-dir", default=str(DEFAULT_INDEX_DIR), help="Index directory path")
    parser.add_argument("--model", default=None, help="Override model")
    parser.add_argument("--top-k", type=int, default=5)
    args = parser.parse_args()

    index_dir = Path(args.index_dir).resolve()
    if not index_dir.exists():
        print(f"Index directory not found: {index_dir}", file=sys.stderr)
        return 1

    # Load metadata
    metadata_path = index_dir / "metadata.json"
    if not metadata_path.exists():
        print(f"Metadata not found: {metadata_path}", file=sys.stderr)
        return 1

    with open(metadata_path) as f:
        metadata = json.load(f)

    if not metadata.get("documents"):
        print("Index is empty.", file=sys.stderr)
        return 1

    # Load FAISS index
    index_path = index_dir / "index.faiss"
    if not index_path.exists():
        print(f"FAISS index not found: {index_path}", file=sys.stderr)
        return 1

    faiss_index = faiss.read_index(str(index_path))
    documents = metadata["documents"]
    model_name = args.model or metadata.get("model", DEFAULT_MODEL)

    # Encode query
    model = SentenceTransformer(model_name)
    query_embedding = model.encode([args.query], normalize_embeddings=True)[0]

    # Search - FAISS returns distances (L2), so smaller is better
    query_embedding = np.asarray([query_embedding], dtype=np.float32)
    distances, indices = faiss_index.search(query_embedding, args.top_k)
    distances = distances[0]
    indices = indices[0]

    for rank, idx in enumerate(indices, start=1):
        if idx < 0 or idx >= len(documents):
            continue
        doc = documents[idx]
        snippet = " ".join(doc["content"].split())
        if len(snippet) > 240:
            snippet = snippet[:237] + "..."
        print(
            f"{rank}. {doc['path']}:{doc['start_line']}-{doc['end_line']} (distance={distances[rank-1]:.3f})\n"
            f"   {snippet}"
        )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
