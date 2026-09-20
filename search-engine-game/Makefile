CORPUS := $(shell pwd)/corpus.json
export CORPUS

WIKI_SRC := "https://www.dropbox.com/s/wwnfnu441w1ec9p/wiki-articles.json.bz2"
CORPUS_LIMIT ?= 1000

COMMANDS ?= TOP_10 TOP_100 TOP_1000 TOP_10_COUNT TOP_100_COUNT TOP_1000_COUNT COUNT
export COMMANDS

ENGINES ?= lead plumb tin
QUERIES ?= queries.txt
PORT ?= 8080
WARMUP_TIME ?= 2
NUM_ITER ?= 3
export WARMUP_TIME
export NUM_ITER

.PHONY: help all corpus clean compile index bench serve

help:
	@echo "Available commands: corpus, compile, index, bench, clean, serve"

all: index

corpus:
	@echo "--- Pulling Wikipedia articles locally and transforming (limit: $(CORPUS_LIMIT)) ---"
	@if curl -sI -L "$(WIKI_SRC)" >/dev/null 2>&1; then \
		curl -# -L "$(WIKI_SRC)" | bunzip2 -c | python3 corpus_transform.py $(CORPUS_LIMIT) > $(CORPUS) ; \
	else \
		echo "Warning: Download failed, generating local fallback corpus." ; \
		python3 -c 'import json; [print(json.dumps({"id": f"http://en.wikipedia.org/wiki/{i}", "text": f"document {i} american history science technology california United States", "sort_field": i})) for i in range(100)]' > $(CORPUS) ; \
	fi
	@echo "Corpus created at $(CORPUS) with $$(wc -l < $(CORPUS)) documents."

clean:
	@echo "--- Cleaning directories ---"
	@rm -f results.json $(CORPUS)
	@for engine in $(ENGINES); do \
		if [ -d engines/$$engine ]; then \
			$(MAKE) -C engines/$$engine clean || true ; \
		fi \
	done

compile:
	@echo "--- Compiling engines ---"
	@for engine in $(ENGINES); do \
		if [ -d engines/$$engine ]; then \
			$(MAKE) -C engines/$$engine compile || true ; \
		fi \
	done

index:
	@echo "--- Indexing corpus across engines ---"
	@for engine in $(ENGINES); do \
		if [ -d engines/$$engine ]; then \
			echo "Indexing $$engine..."; \
			$(MAKE) -C engines/$$engine index ; \
		fi \
	done

bench:
	@echo "--- Benchmarking engines ---"
	@python3 src/client.py "$(QUERIES)" $(ENGINES)

serve:
	@echo "--- Serving results on port $(PORT) ---"
	@python3 -m http.server $(PORT)
