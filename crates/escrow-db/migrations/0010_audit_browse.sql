-- target-prefix filter for GET /admin/audit (audit_log_lookup covers action+target equality)
CREATE INDEX audit_log_target_prefix ON audit_log (target text_pattern_ops);
