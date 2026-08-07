#!/usr/bin/env python3
"""Read-only SQLite evidence collector for browser diagnostics E2E tests."""

from __future__ import annotations

import argparse
import json
import sqlite3
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("database")
    args = parser.parse_args()

    database = Path(args.database).resolve()
    uri = f"file:{database.as_posix()}?mode=ro"
    connection = sqlite3.connect(uri, uri=True, timeout=5)
    connection.row_factory = sqlite3.Row
    try:
        requests = connection.execute(
            """
            SELECT event_id, host, url, resource_type, mime, status_code,
                   transferred_bytes, declared_bytes, protocol, from_cache,
                   page_url, initiator, occurred_at_ms
            FROM browser_request_event
            ORDER BY COALESCE(transferred_bytes, declared_bytes, 0) DESC,
                     occurred_at_ms DESC
            LIMIT 100
            """
        ).fetchall()
        downloads = connection.execute(
            """
            SELECT event_id, host, url, final_url, filename, mime, total_bytes,
                   state, danger, exists_local, updated_at_ms
            FROM browser_download_event
            ORDER BY updated_at_ms DESC
            LIMIT 100
            """
        ).fetchall()
    finally:
        connection.close()

    request_rows = [dict(row) for row in requests]
    download_rows = [dict(row) for row in downloads]
    positive = [row for row in request_rows if (row.get("transferred_bytes") or 0) > 0]
    largest = positive[0] if positive else (request_rows[0] if request_rows else None)
    print(
        json.dumps(
            {
                "database": str(database),
                "request_count": len(request_rows),
                "download_count": len(download_rows),
                "positive_transferred_count": len(positive),
                "largest_request": largest,
                "requests": request_rows,
                "downloads": download_rows,
            },
            ensure_ascii=False,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
