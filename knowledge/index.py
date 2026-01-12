#!/usr/bin/env python3
import argparse
import json
import os
import shutil
import sys
import time
from pathlib import Path

import faiss
import numpy as np
from sentence_transformers import SentenceTransformer
from tqdm import tqdm

DEFAULT_MODEL = "sentence-transformers/all-MiniLM-L6-v2"
DEFAULT_ROOT = Path.cwd()  # Where the script was run from
DEFAULT_INDEX_DIR = Path.cwd() / "index_data"  # Where the script was run from
DEFAULT_CHUNK_CHARS = 1200
DEFAULT_OVERLAP_LINES = 3
DEFAULT_BATCH_SIZE = 32  # Small batch size to avoid memory buildup

DEFAULT_EXTENSIONS = {
    ".rs",
    ".md",
}

ALWAYS_INCLUDE = set()

IGNORE_DIRS = {
    ".git",
    ".idea",
    ".cache",
    ".venv",
    "venv",
    "build",
    "builddir",
    "target",
    "third_party",
    "iso",
    "test_efi_dir",
    "EFI",
    "boot_fat",
    "node_modules",
    "knowledge",
}


def is_text_file(path: Path) -> bool:
    try:
        with path.open("rb") as handle:
            chunk = handle.read(2048)
        return b"\x00" not in chunk
    except OSError:
        return False


def iter_source_files(root: Path, extensions, always_include):
    for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
        dirnames[:] = [d for d in dirnames if d not in IGNORE_DIRS]
        for filename in filenames:
            path = Path(dirpath) / filename
            rel_path = path.relative_to(root)
            if filename in always_include or path.suffix in extensions:
                if is_text_file(path):
                    yield rel_path, path


def chunk_lines(lines, max_chars, overlap_lines):
    start = 0
    total = len(lines)
    while start < total:
        current_lines = []
        length = 0
        idx = start
        while idx < total:
            line_len = len(lines[idx]) + 1
            if current_lines and length + line_len > max_chars:
                break
            current_lines.append(lines[idx])
            length += line_len
            idx += 1
        if not current_lines:
            current_lines.append(lines[start])
            idx = start + 1
        chunk = "\n".join(current_lines).strip()
        if chunk:
            yield chunk, start + 1, idx
        if idx >= total:
            break
        start = max(0, idx - overlap_lines)


def main() -> int:
    parser = argparse.ArgumentParser(description="Build a semantic search index.")
    parser.add_argument("--root", default=str(DEFAULT_ROOT), help="Repository root to index")
    parser.add_argument("--index-dir", default=str(DEFAULT_INDEX_DIR), help="Index directory path")
    parser.add_argument("--model", default=DEFAULT_MODEL, help="SentenceTransformer model")
    parser.add_argument("--chunk-chars", type=int, default=DEFAULT_CHUNK_CHARS)
    parser.add_argument("--overlap-lines", type=int, default=DEFAULT_OVERLAP_LINES)
    parser.add_argument("--batch-size", type=int, default=DEFAULT_BATCH_SIZE)
    args = parser.parse_args()

    root = Path(args.root).resolve()
    index_dir = Path(args.index_dir).resolve()

    if not root.exists():
        print(f"Root not found: {root}", file=sys.stderr)
        return 1

    if index_dir.exists():
        print(f"Removing old index at: {index_dir}", file=sys.stderr, flush=True)
        shutil.rmtree(index_dir)

    extensions = DEFAULT_EXTENSIONS
    always_include = ALWAYS_INCLUDE

    print(f"Indexing source root: {root}", file=sys.stderr, flush=True)
    print(f"Loading model: {args.model}", file=sys.stderr, flush=True)
    model = SentenceTransformer(args.model)

    # Discover files
    print("Discovering source files...", file=sys.stderr, flush=True)
    source_files = list(iter_source_files(root, extensions, always_include))
    print(f"  Found {len(source_files)} source files", file=sys.stderr, flush=True)

    # Initialize FAISS index (will be created after first batch)
    faiss_index = None
    embedding_dim = None
    all_documents = []

    # Process files and batches
    batch_texts = []
    batch_docs = []
    total_chunks = 0

    print("Processing files and encoding...", file=sys.stderr, flush=True)
    for file_idx, (rel_path, path) in enumerate(tqdm(source_files, desc="Files", unit="file"), 1):
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError as e:
            print(f"\n  Warning: Could not read {rel_path}: {e}", file=sys.stderr, flush=True)
            continue

        lines = text.splitlines()
        mtime = path.stat().st_mtime

        for chunk, start_line, end_line in chunk_lines(lines, args.chunk_chars, args.overlap_lines):
            batch_texts.append(chunk)
            batch_docs.append(
                {
                    "path": str(rel_path),
                    "start_line": start_line,
                    "end_line": end_line,
                    "content": chunk,
                    "mtime": mtime,
                }
            )
            total_chunks += 1

            # When batch is full, encode and add to index
            if len(batch_texts) >= args.batch_size:
                embeddings = model.encode(
                    batch_texts,
                    batch_size=args.batch_size,
                    show_progress_bar=False,
                    normalize_embeddings=True,
                )
                embeddings = np.asarray(embeddings, dtype=np.float32)

                # Initialize index on first batch
                if faiss_index is None:
                    embedding_dim = embeddings.shape[1]
                    faiss_index = faiss.IndexFlatL2(embedding_dim)
                    print(f"  Initialized FAISS index (dim={embedding_dim})", file=sys.stderr, flush=True)

                faiss_index.add(embeddings)
                all_documents.extend(batch_docs)

                # Clear batch
                batch_texts = []
                batch_docs = []

                print(
                    f"  Processed {total_chunks} chunks, index size: {faiss_index.ntotal}",
                    file=sys.stderr,
                    flush=True,
                )

    # Process remaining batch
    if batch_texts:
        print("Encoding final batch...", file=sys.stderr, flush=True)
        embeddings = model.encode(
            batch_texts,
            batch_size=args.batch_size,
            show_progress_bar=False,
            normalize_embeddings=True,
        )
        embeddings = np.asarray(embeddings, dtype=np.float32)

        if faiss_index is None:
            embedding_dim = embeddings.shape[1]
            faiss_index = faiss.IndexFlatL2(embedding_dim)

        faiss_index.add(embeddings)
        all_documents.extend(batch_docs)

    if faiss_index is None:
        print("No documents found.", file=sys.stderr, flush=True)
        return 1

    # Save index
    print(f"Saving index to: {index_dir}", file=sys.stderr, flush=True)
    index_dir.mkdir(parents=True, exist_ok=True)

    faiss.write_index(faiss_index, str(index_dir / "index.faiss"))

    data = {
        "model": args.model,
        "embedding_dim": embedding_dim,
        "chunk_chars": args.chunk_chars,
        "overlap_lines": args.overlap_lines,
        "indexed_at": int(time.time()),
        "documents": all_documents,
    }

    with open(index_dir / "metadata.json", "w") as f:
        json.dump(data, f, indent=2)

    print(f"\n✓ Successfully indexed {len(all_documents)} chunks into {index_dir}", file=sys.stderr, flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
