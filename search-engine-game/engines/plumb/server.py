import os
import sys
import psycopg2

DSN = os.environ.get('PLUMB_DSN', 'host=127.0.0.1 port=28818 dbname=postgres')

def translate_query(q):
    q = q.strip()
    if not q:
        return ""
    if q.startswith('"') and q.endswith('"'):
        return q
    tokens = q.split()
    if all(t.startswith('+') for t in tokens):
        terms = [t[1:] for t in tokens if len(t) > 1]
        return " AND ".join(terms)
    elif len(tokens) > 1 and not any(t.startswith('+') for t in tokens):
        return " OR ".join(tokens)
    else:
        terms = [t[1:] if t.startswith('+') else t for t in tokens]
        return " ".join(terms)

def main():
    conn = psycopg2.connect(DSN)
    conn.autocommit = True
    cur = conn.cursor()
    cur.execute("SET enable_seqscan = off;")

    # Prepare statements
    cur.execute("PREPARE prep_count (text) AS SELECT count(*) FROM documents_plumb WHERE body ~~> $1;")
    cur.execute("PREPARE prep_top (text, int) AS SELECT id FROM documents_plumb WHERE body ~~> $1 LIMIT $2;")

    limit_map = {"TOP_10": 10, "TOP_100": 100, "TOP_1000": 1000}

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        parts = line.split('\t', 1)
        if len(parts) < 2:
            print("UNSUPPORTED")
            sys.stdout.flush()
            continue
        cmd, raw_query = parts[0].strip(), parts[1].strip()
        tq = translate_query(raw_query)

        try:
            if cmd == "COUNT":
                cur.execute("EXECUTE prep_count (%s);", (tq,))
                cnt = cur.fetchone()[0]
                print(cnt)
            elif cmd in limit_map:
                limit = limit_map[cmd]
                cur.execute("EXECUTE prep_top (%s, %s);", (tq, limit))
                cur.fetchall()
                print(1)
            elif cmd in ("TOP_10_COUNT", "TOP_100_COUNT", "TOP_1000_COUNT"):
                cur.execute("EXECUTE prep_count (%s);", (tq,))
                cnt = cur.fetchone()[0]
                print(cnt)
            else:
                print("UNSUPPORTED")
            sys.stdout.flush()
        except Exception as e:
            print("UNSUPPORTED")
            sys.stdout.flush()

    cur.close()
    conn.close()

if __name__ == "__main__":
    main()
