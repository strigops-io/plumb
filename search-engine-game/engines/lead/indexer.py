import json
import os
import sys
import time
import psycopg2

DSN = os.environ.get('LEAD_DSN') or os.environ.get('TEST_TIN_PLANETSCALE') or 'host=127.0.0.1 port=28818 dbname=postgres'
if 'psdb.cloud' in DSN and 'sslmode=' not in DSN:
    DSN += '?sslmode=require' if '?' not in DSN else '&sslmode=require'

def main():
    conn = psycopg2.connect(DSN)
    cur = conn.cursor()

    cur.execute("SELECT name FROM pg_available_extensions WHERE name IN ('tin', 'lead');")
    exts = [r[0] for r in cur.fetchall()]
    ext_name = 'tin' if 'tin' in exts else ('lead' if 'lead' in exts else 'tin')

    cur.execute(f"CREATE EXTENSION IF NOT EXISTS {ext_name};")
    cur.execute("DROP TABLE IF EXISTS documents_lead CASCADE;")
    cur.execute("CREATE TABLE documents_lead (id text PRIMARY KEY, body text);")
    conn.commit()

    batch_size = 250
    batch = []
    total = 0

    print("Indexing documents for Lead...", file=sys.stderr)
    for line in sys.stdin:
        if not line.strip():
            continue
        try:
            doc = json.loads(line)
        except Exception:
            continue
        batch.append((doc["id"], doc["text"]))
        if len(batch) >= batch_size:
            query = "INSERT INTO documents_lead (id, body) VALUES " + ','.join(['(%s,%s)'] * len(batch)) + " ON CONFLICT (id) DO NOTHING;"
            args_flat = [item for sublist in batch for item in sublist]
            cur.execute(query, args_flat)
            conn.commit()
            total += len(batch)
            batch = []

    if batch:
        query = "INSERT INTO documents_lead (id, body) VALUES " + ','.join(['(%s,%s)'] * len(batch)) + " ON CONFLICT (id) DO NOTHING;"
        args_flat = [item for sublist in batch for item in sublist]
        cur.execute(query, args_flat)
        conn.commit()
        total += len(batch)

    print(f"Inserted {total} documents into documents_lead. Creating index...", file=sys.stderr)
    start_time = time.time()
    cur.execute(f"CREATE INDEX documents_lead_idx ON documents_lead USING {ext_name} (body);")
    conn.commit()
    elapsed = time.time() - start_time
    print(f"Lead index created successfully in {elapsed:.2f}s.", file=sys.stderr)
    cur.close()
    conn.close()

if __name__ == "__main__":
    main()
