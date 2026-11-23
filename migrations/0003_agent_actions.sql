ALTER TABLE agent_actions ADD COLUMN tool_name TEXT;
ALTER TABLE agent_actions ADD COLUMN args_json TEXT;
ALTER TABLE agent_actions ADD COLUMN expires_at INTEGER;
