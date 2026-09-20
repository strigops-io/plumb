# Search Engine Game

A benchmark suite based on `search-benchmark-game` for comparing PostgreSQL-based text search implementations:

- **lead**: Public PlanetScale Lead / PostgreSQL extension using `==>` operator / `tin` AM locally.
- **plumb**: Plumb PostgreSQL extension using `~~>` operator / `plumb` AM locally.
- **tin**: Live PlanetScale TIN instance using `==>` operator / `tin` AM over TLS.

## Sizing & Limits (PS-5 Compatibility)

The PlanetScale PS-5 instance specification is:
- **vCPU**: 1/16 vCPU
- **Memory**: 512 MB
- **Storage**: 10 GB

To ensure the benchmark runs efficiently and does not exceed memory or disk limits on PS-5 instances, the default local corpus size is capped (default: 10,000 Wikipedia articles). You can adjust `CORPUS_LIMIT` as needed:

```sh
make corpus CORPUS_LIMIT=10000
make index ENGINES="lead plumb tin"
make bench ENGINES="lead plumb tin"
```

## Available Makefile Commands

- `make corpus`: Downloads Wikipedia articles and transforms them into `corpus.json`.
- `make compile`: Compiles engine indexers/servers.
- `make index`: Populates indexes for specified `ENGINES`.
- `make bench`: Executes queries and measures latency across engines, saving output to `results.json`.
- `make clean`: Cleans indexes and results.
