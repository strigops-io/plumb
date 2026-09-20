.PHONY: clean compile index serve

clean:
	@python3 -c "import psycopg2, os; conn=psycopg2.connect(os.environ.get('PLUMB_DSN', 'host=127.0.0.1 port=28818 dbname=postgres')); cur=conn.cursor(); cur.execute('DROP TABLE IF EXISTS documents_plumb CASCADE;'); conn.commit(); conn.close()" 2>/dev/null || true

compile:
	@echo "Plumb python runner ready."

index:
	@python3 indexer.py < $(CORPUS)

serve:
	@python3 server.py
