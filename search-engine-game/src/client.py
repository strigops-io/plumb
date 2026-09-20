import subprocess
import os
from os import path
import time
import json
import random
import sys

COMMANDS = os.environ.get('COMMANDS', 'TOP_10 TOP_100 TOP_1000 TOP_10_COUNT TOP_100_COUNT TOP_1000_COUNT COUNT').split(' ')

class SearchClient:
    def __init__(self, engine):
        self.engine = engine
        dirname = os.path.split(os.path.abspath(__file__))[0]
        dirname = path.dirname(dirname)
        dirname = path.join(dirname, "engines")
        cwd = path.join(dirname, engine)
        print(f"Starting server for engine: {engine} in {cwd}")
        self.process = subprocess.Popen(
            ["make", "--no-print-directory", "serve"],
            cwd=cwd,
            stdout=subprocess.PIPE,
            stdin=subprocess.PIPE
        )

    def query(self, query, command):
        query_line = "%s\t%s\n" % (command, query)
        self.process.stdin.write(query_line.encode("utf-8"))
        self.process.stdin.flush()
        recv = self.process.stdout.readline().strip()
        if not recv or recv == b"UNSUPPORTED":
            return None
        try:
            return int(recv)
        except ValueError:
            print(f"Error parsing response: {recv}", file=sys.stderr)
            return None

    def close(self):
        try:
            self.process.stdin.close()
            self.process.stdout.close()
            self.process.terminate()
            self.process.wait(timeout=5)
        except Exception:
            pass

def drive(queries, client, command):
    for query in queries:
        start = time.monotonic()
        count = client.query(query.query, command)
        stop = time.monotonic()
        duration = int((stop - start) * 1e6) # microseconds
        yield (query, count, duration)

class Query(object):
    def __init__(self, query, tags):
        self.query = query
        self.tags = tags

def read_queries(query_path):
    with open(query_path, 'r', encoding='utf-8') as f:
        for q in f:
            if not q.strip():
                continue
            c = json.loads(q)
            yield Query(c["query"], c.get("tags", []))

WARMUP_TIME = int(os.environ.get('WARMUP_TIME', '2'))
NUM_ITER = int(os.environ.get('NUM_ITER', '3'))

if __name__ == "__main__":
    random.seed(2)
    query_path = sys.argv[1]
    engines = sys.argv[2:]
    queries = list(read_queries(query_path))

    details = {}
    for engine in engines:
        dirname = os.path.split(os.path.abspath(__file__))[0]
        dirname = path.dirname(dirname)
        dirname = path.join(dirname, "engines")
        details_file = path.join(dirname, engine, "details.json")
        if os.path.exists(details_file):
            with open(details_file, "r") as f:
                details[engine] = json.loads(f.read())
        else:
            details[engine] = []

    results = {}
    for command in COMMANDS:
        results_commands = {}
        for engine in engines:
            engine_results = []
            query_idx = {}
            for query in queries:
                query_result = {
                    "query": query.query,
                    "tags": query.tags,
                    "count": 0,
                    "duration": []
                }
                query_idx[query.query] = query_result
                engine_results.append(query_result)
            print("======================")
            print("BENCHMARKING %s %s" % (engine, command))
            search_client = SearchClient(engine)
            queries_shuffled = list(queries[:])
            random.seed(2)
            random.shuffle(queries_shuffled)
            warmup_start = time.monotonic()
            while True:
                for _ in drive(queries_shuffled, search_client, command):
                    pass
                if (time.monotonic() - warmup_start) >= WARMUP_TIME:
                    break
            for i in range(NUM_ITER):
                for (query, count, duration) in drive(queries_shuffled, search_client, command):
                    if count is None:
                        query_idx[query.query] = {"count": -1, "duration": []}
                    else:
                        query_idx[query.query]["count"] = count
                        query_idx[query.query]["duration"].append(duration)
            for query in engine_results:
                query["duration"].sort()
            results_commands[engine] = engine_results
            search_client.close()
        results[command] = results_commands

    output_path = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "results.json")
    with open(output_path, "w", encoding="utf-8") as f:
        json.dump({"details": details, "results": results}, f, indent=2)
    print(f"Results saved to {output_path}")
