-- Per-profile tool guard (SOUL §19): a programmable classifier (Boa JS and/or
-- LLM) that gates every tool call a profile makes, layered on top of the static
-- capability grant. Stored as a nullable JSONB blob mirroring `ToolGuard`
-- (`{ script?, llm?, on_error }`); NULL leaves the profile gated only by its
-- capabilities. Because subagents are profiles, this covers delegated runs too.
ALTER TABLE agent_profiles ADD COLUMN guard JSONB;
