import json
import os
import sys
import psycopg2

DSN = os.environ.get('PLUMB_DSN', 'host=127.0.0.1 port=28818 dbname=postgres')

def main():
    conn = psycopg2.connect(DSN)
    cur = conn.cursor()

    cur.execute("CREATE EXTENSION IF NOT EXISTS plumb;")
    cur.execute("DROP TABLE IF EXISTS documents_plumb CASCADE;")
    cur.execute("CREATE TABLE documents_plumb (id text PRIMARY KEY, body text);")
    conn.commit()

    batch_size = 2000
    batch = []
    total = 0

    print("Indexing documents for Plumb...", file=sys.stderr)
    for line in sys.stdin:
        if not line.strip():
            continue
        try:
            doc = json.loads(line)
        except Exception:
            continue
        batch.append((doc["id"], doc["text"]))
        if len(batch) >= batch_size:
            args_flat = [item for sublist in batch for item in sublist]
            query = "INSERT INTO documents_plumb (id, body) VALUES " + ','.join(['(%s,%s)'] * len(batch)) + " ON CONFLICT (id) DO NOTHING;"
            cur.execute(query, args_flat)
            conn.commit()
            total += len(batch)
            batch = []

    if batch:
        query = "INSERT INTO documents_plumb (id, body) VALUES " + ','.join(['(%s,%s)'] * len(batch)) + " ON CONFLICT (id) DO NOTHING;"
        args_flat = [item for sublist in batch for item in sublist]
        cur.execute(query, args_flat)
        conn.commit()
        total += len(batch)

    print(f"Inserted {total} documents into documents_plumb. Creating plumb index...", file=sys.stderr)
    cur.execute("CREATE INDEX documents_plumb_idx ON documents_plumb USING plumb (body) WITH (storage='heap');")
    conn.commit()
    print("Plumb index created successfully.", file=sys.stderr)
    cur.close()
    conn.close()

if __name__ == "__main__":
    main()
