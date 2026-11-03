-- schema
CREATE TABLE users (
    id SERIAL PRIMARY KEY,
    name VARCHAR(100) NOT NULL,
    created_at TIMESTAMP DEFAULT now()
);

CREATE VIEW active_users AS
SELECT id, name FROM users WHERE created_at > '2024-01-01';

CREATE INDEX idx_users_name ON users (name);

/* query */
SELECT u.id, COUNT(*) AS n
FROM users u
JOIN orders o ON o.user_id = u.id
WHERE u.name LIKE 'a%' AND o.total >= 10.5
GROUP BY u.id
ORDER BY n DESC
LIMIT 10;
