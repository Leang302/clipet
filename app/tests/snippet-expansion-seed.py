import json, os, sqlite3, sys, time

db = os.path.join(os.environ["APPDATA"], "com.clipet.snippetuxtest", "snippets.sqlite3")
assert "snippetuxtest" in db
undo = sys.argv[1] == "undo-on" if len(sys.argv) > 1 else True
c = sqlite3.connect(db)
now = int(time.time() * 1000)
c.execute("DELETE FROM snippets")
c.execute(
    "INSERT INTO snippets (id,name,keyword,content,content_type,enabled,favorite,use_count,"
    "created_at,updated_at,suffix,parameters,variants) "
    "VALUES ('t-name','Name','name','Keo Kimleang','text',1,0,0,?,?,'exact','[]','{}')",
    (now, now),
)
settings = dict(enabled=True, prefix=";", trigger_mode="space", escape_enabled=True,
                case_sensitive=True, default_pause_minutes=15, undo_backspace=undo)
c.execute(
    "INSERT INTO app_settings(key,value) VALUES('snippets.settings',?) "
    "ON CONFLICT(key) DO UPDATE SET value=excluded.value",
    (json.dumps(settings),),
)
c.commit()
print(c.execute("select keyword, content, suffix from snippets").fetchall())
print(c.execute("select value from app_settings where key='snippets.settings'").fetchone())
