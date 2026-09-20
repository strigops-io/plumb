import fileinput
import json
import random
import re
import sys

PTN = re.compile("[^a-zA-Z0-9]+")

random.seed(42)

def transform(text):
    return PTN.sub(" ", text.lower())

def main():
    limit = int(sys.argv[1]) if len(sys.argv) > 1 else None
    count = 0
    for line in sys.stdin:
        if limit is not None and count >= limit:
            break
        try:
            doc = json.loads(line)
        except Exception:
            continue

        url = doc.get("url", "")
        body = doc.get("body", "")
        if not url or not body:
            continue

        doc_transformed = {
            "id": url,
            "text": transform(body),
            "sort_field": random.randint(0, 2**32 - 1)
        }

        print(json.dumps(doc_transformed))
        count += 1

if __name__ == "__main__":
    main()
