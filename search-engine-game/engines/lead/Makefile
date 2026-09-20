.PHONY: clean compile index serve

clean:
	@python3 -c "import psycopg2, os; dsn=os.environ.get('LEAD_DSN', os.environ.get('TEST_TIN_PLANETSCALE', 'host=127.0.0.1 port=28818 dbname=postgres')); conn=psycopg2.connect(dsn); cur=conn.cursor(); cur.execute('DROP TABLE IF EXISTS documents_lead CASCADE;'); conn.commit(); conn.close()" 2>/dev/null || true

compile:
	@echo "Lead python runner ready."

index:
	@python3 indexer.py < $(CORPUS)

serve:
	@python3 server.py
