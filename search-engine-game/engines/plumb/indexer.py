import json
import os
import sys
import time
import psycopg2

DSN = os.environ.get('PLUMB_DSN', 'host=127.0.0.1 port=28818 dbname=postgres')

def main():
    conn = psycopg2.connect(DSN)
    cur = conn.cursor()

    cur.execute("CREATE EXTENSION IF NOT EXISTS plumb;")
    cur.execute("DROP TABLE IF EXISTS documents_plumb CASCADE;")
    cur.execute("CREATE TABLE documents_plumb (id text PRIMARY KEY, body text);")
    conn.commit()

    batch = []
    print("Indexing documents for Plumb using default postings_v1 storage engine...", file=sys.stderr)
    for line in sys.stdin:
        if not line.strip():
            continue
        try:
            doc = json.loads(line)
        except Exception:
            continue
        batch.append((doc["id"], doc["text"]))

    if batch:
        query = "INSERT INTO documents_plumb (id, body) VALUES " + ','.join(['(%s,%s)'] * len(batch)) + " ON CONFLICT (id) DO NOTHING;"
        args_flat = [item for sublist in batch for item in sublist]
        cur.execute(query, args_flat)
        conn.commit()

    print(f"Inserted {len(batch)} documents into documents_plumb. Creating plumb (postings_v1) index...", file=sys.stderr)
    start_time = time.time()
    cur.execute("CREATE INDEX documents_plumb_idx ON documents_plumb USING plumb (body);")
    conn.commit()
    elapsed = time.time() - start_time
    print(f"Plumb (postings_v1) index created successfully in {elapsed:.2f}s.", file=sys.stderr)
    cur.close()
    conn.close()

if __name__ == "__main__":
    main()
