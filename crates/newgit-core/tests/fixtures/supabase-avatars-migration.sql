-- A bucket made the usual way, in a migration. `supabase db reset` replays
-- it, so a restore meets this row before it loads the dump's copy (#241).
create table public.profiles (id uuid primary key references auth.users (id), username text);
insert into storage.buckets (id, name, public) values ('avatars', 'avatars', true);
