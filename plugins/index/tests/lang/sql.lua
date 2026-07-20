local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

case("sql_create_table_with_columns", function()
  local src = [[
CREATE TABLE public.users (
    id INT PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    email VARCHAR(255)
);
]]
  local out = idx(src, "sql")
  has(out, {
    "classes:",
    "TABLE public.users",
    "id INT",
    "name VARCHAR(255)",
    "email VARCHAR(255)",
  })
end)

case("sql_create_view", function()
  local src = "CREATE VIEW active_users AS SELECT id, name FROM users WHERE active = true;\n"
  local out = idx(src, "sql")
  has(out, { "types:", "VIEW active_users", "SELECT id, name FROM users" })
end)

case("sql_create_materialized_view", function()
  local src = "CREATE MATERIALIZED VIEW mv_totals AS SELECT sum(amount) FROM orders;\n"
  local out = idx(src, "sql")
  has(out, { "MATERIALIZED VIEW mv_totals", "SELECT sum(amount) FROM orders" })
end)

case("sql_create_function_with_args_and_language", function()
  local src = [[
CREATE FUNCTION add_one(x INT) RETURNS INT LANGUAGE plpgsql AS $$
BEGIN
  RETURN x + 1;
END;
$$;
]]
  local out = idx(src, "sql")
  has(out, { "fns:", "FUNCTION add_one(x INT)", "LANGUAGE plpgsql" })
end)

case("sql_create_function_multiple_args", function()
  local src = [[
CREATE FUNCTION full_name(first_name TEXT, last_name TEXT) RETURNS TEXT LANGUAGE sql AS $$
  SELECT first_name || ' ' || last_name;
$$;
]]
  local out = idx(src, "sql")
  has(out, { "FUNCTION full_name(first_name TEXT, last_name TEXT)", "LANGUAGE sql" })
end)

case("sql_create_index", function()
  local src = "CREATE INDEX idx_users_email ON users (email);\n"
  local out = idx(src, "sql")
  has(out, { "INDEX idx_users_email ON users(email)" })
end)

case("sql_create_index_with_modifiers", function()
  local src = "CREATE UNIQUE INDEX IF NOT EXISTS idx_users_email ON users (email);\n"
  local out = idx(src, "sql")
  has(out, { "INDEX idx_users_email ON users(email)" })
end)

case("sql_create_index_unnamed", function()
  local src = "CREATE INDEX ON users (email, name);\n"
  local out = idx(src, "sql")
  has(out, { "INDEX ON users(email, name)" })
end)

case("sql_create_index_schema_qualified", function()
  local src = "CREATE INDEX public.idx_users_email ON public.users (email);\n"
  local out = idx(src, "sql")
  has(out, { "INDEX ON public.users(email)" })
end)

case("sql_create_index_quoted_schema", function()
  local src = 'CREATE INDEX "public"."idx_users_email" ON public.users (email);\n'
  local out = idx(src, "sql")
  has(out, { "INDEX ON public.users(email)" })
end)

case("sql_create_trigger", function()
  local src = [[
CREATE TRIGGER set_updated_at
BEFORE UPDATE ON users
FOR EACH ROW
EXECUTE FUNCTION update_timestamp();
]]
  local out = idx(src, "sql")
  has(out, { "TRIGGER set_updated_at ON users" })
end)

case("sql_create_type", function()
  local src = "CREATE TYPE point AS (x INT, y INT);\n"
  local out = idx(src, "sql")
  has(out, { "types:", "TYPE point", "x INT", "y INT" })
end)

case("sql_create_type_enum", function()
  local src = "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');\n"
  local out = idx(src, "sql")
  has(out, { "TYPE mood", "'sad'", "'ok'", "'happy'" })
end)

case("sql_create_schema", function()
  local src = "CREATE SCHEMA analytics;\n"
  local out = idx(src, "sql")
  has(out, { "mod:", "analytics" })
end)

case("sql_line_comment_attaches_to_following_statement", function()
  local src = "-- Users table\nCREATE TABLE users (id INT);\n"
  local out = idx(src, "sql")
  has(out, { "TABLE users [1-2]" })
end)

case("sql_block_comment_attaches_to_following_statement", function()
  local src = "/* Users table */\nCREATE TABLE users (id INT);\n"
  local out = idx(src, "sql")
  has(out, { "TABLE users [1-2]" })
end)

case("sql_dml_statements_produce_no_entries", function()
  local cases = {
    "SELECT * FROM users;\n",
    "INSERT INTO users (id) VALUES (1);\n",
    "UPDATE users SET name = 'x' WHERE id = 1;\n",
    "DELETE FROM users WHERE id = 1;\n",
  }
  for _, src in ipairs(cases) do
    local out = idx(src, "sql")
    lacks(out, { "classes:", "types:", "fns:", "mod:" })
  end
end)

case("sql_alter_and_drop_produce_no_entries", function()
  local cases = {
    "ALTER TABLE users ADD COLUMN age INT;\n",
    "DROP TABLE users;\n",
  }
  for _, src in ipairs(cases) do
    local out = idx(src, "sql")
    lacks(out, { "classes:", "types:", "fns:", "mod:" })
  end
end)

case("sql_transaction_with_ddl", function()
  local src = [[
BEGIN;
CREATE TABLE orders (
  id INT PRIMARY KEY,
  amount NUMERIC
);
COMMIT;
]]
  local out = idx(src, "sql")
  has(out, { "classes:", "TABLE orders", "id INT", "amount NUMERIC" })
end)

case("sql_transaction_with_explicit_keyword", function()
  local src = [[
BEGIN TRANSACTION;
CREATE TABLE logs (
  id INT,
  message TEXT
);
COMMIT TRANSACTION;
]]
  local out = idx(src, "sql")
  has(out, { "TABLE logs", "id INT", "message TEXT" })
end)

case("sql_create_if_not_exists", function()
  local src = "CREATE TABLE IF NOT EXISTS users (id INT PRIMARY KEY);\n"
  local out = idx(src, "sql")
  has(out, { "TABLE users", "id INT" })
end)

case("sql_multiple_ddl_in_transaction", function()
  local src = [[
BEGIN;
CREATE TABLE a (id INT);
CREATE TABLE b (id INT);
COMMIT;
]]
  local out = idx(src, "sql")
  has(out, { "TABLE a", "TABLE b" })
end)

case("sql_transaction_with_comments", function()
  local src = [[
-- start tx
BEGIN;
/* create the orders table */
CREATE TABLE orders (id INT);
COMMIT;
]]
  local out = idx(src, "sql")
  has(out, { "TABLE orders" })
end)

case("sql_error_recovery_ignores_bad_statement", function()
  local src = [[
CREATE TABLE good (id INT);
CREATE TABLE (missing_name;
CREATE TABLE also_good (name TEXT);
]]
  local out = idx(src, "sql")
  has(out, { "TABLE good", "TABLE also_good", "name TEXT" })
  lacks(out, { "missing_name" })
end)
