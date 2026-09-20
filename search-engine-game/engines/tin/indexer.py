import json
import os
import sys
import time
import psycopg2

DSN = os.environ.get('TIN_DSN') or os.environ.get('TEST_TIN_PLANETSCALE') or 'host=127.0.0.1 port=28818 dbname=postgres'
if 'psdb.cloud' in DSN and 'sslmode=' not in DSN:
    DSN += '?sslmode=require' if '?' not in DSN else '&sslmode=require'

def main():
    conn = psycopg2.connect(DSN)
    cur = conn.cursor()

    cur.execute("CREATE EXTENSION IF NOT EXISTS tin;")
    cur.execute("DROP TABLE IF EXISTS documents_tin CASCADE;")
    cur.execute("CREATE TABLE documents_tin (id text PRIMARY KEY, body text);")
    conn.commit()

    # Batch size of 250 rows to fit within PS-5 single-node 512MB RAM and 1/16 vCPU resource limits
    batch_size = 250
    batch = []
    total = 0

    print("Indexing documents for PlanetScale TIN...", file=sys.stderr)
    for line in sys.stdin:
        if not line.strip():
            continue
        try:
            doc = json.loads(line)
        except Exception:
            continue
        batch.append((doc["id"], doc["text"]))
        if len(batch) >= batch_size:
            query = "INSERT INTO documents_tin (id, body) VALUES " + ','.join(['(%s,%s)'] * len(batch)) + " ON CONFLICT (id) DO NOTHING;"
            args_flat = [item for sublist in batch for item in sublist]
            cur.execute(query, args_flat)
            conn.commit()
            total += len(batch)
            batch = []

    if batch:
        query = "INSERT INTO documents_tin (id, body) VALUES " + ','.join(['(%s,%s)'] * len(batch)) + " ON CONFLICT (id) DO NOTHING;"
        args_flat = [item for sublist in batch for item in sublist]
        cur.execute(query, args_flat)
        conn.commit()
        total += len(batch)

    print(f"Inserted {total} documents into documents_tin. Creating TIN index on PlanetScale...", file=sys.stderr)
    start_time = time.time()
    cur.execute("CREATE INDEX documents_tin_idx ON documents_tin USING tin (body);")
    conn.commit()
    elapsed = time.time() - start_time
    print(f"PlanetScale TIN index created successfully in {elapsed:.2f}s.", file=sys.stderr)
    cur.close()
    conn.close()

if __name__ == "__main__":
    main()
