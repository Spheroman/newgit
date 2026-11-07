-- A local user to sign in as, the usual thing to seed. `supabase db reset`
-- runs this after the migrations ([db.seed] is on in config.toml), so a
-- restore meets this row too.
insert into auth.users (id, email) values ('00000000-0000-0000-0000-000000000001', 'seed@example.com');
