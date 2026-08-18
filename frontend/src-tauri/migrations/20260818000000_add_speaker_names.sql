CREATE TABLE IF NOT EXISTS speaker_names (
    meeting_id TEXT NOT NULL,
    speaker TEXT NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY (meeting_id, speaker),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);
