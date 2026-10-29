
CREATE TABLE project_folder_origins (
  project_id TEXT PRIMARY KEY NOT NULL REFERENCES registered_projects(project_id) ON DELETE CASCADE,
  manual INTEGER NOT NULL CHECK(manual IN (0, 1))
);
INSERT INTO project_folder_origins(project_id, manual)
  SELECT project_id, 1 FROM registered_projects;

CREATE TABLE project_folder_providers (
  project_id TEXT NOT NULL REFERENCES registered_projects(project_id) ON DELETE CASCADE,
  provider TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('present', 'absent', 'unknown')),
  last_known_present INTEGER NOT NULL CHECK(last_known_present IN (0, 1)),
  PRIMARY KEY(project_id, provider)
);
CREATE INDEX project_folder_providers_by_provider ON project_folder_providers(provider);

CREATE TABLE project_folder_cwds (
  provider TEXT NOT NULL,
  cwd TEXT NOT NULL,
  project_id TEXT NOT NULL REFERENCES registered_projects(project_id) ON DELETE CASCADE,
  PRIMARY KEY(provider, cwd)
);

CREATE TABLE project_discovery_revisions (
  provider TEXT PRIMARY KEY NOT NULL,
  revision INTEGER NOT NULL CHECK(revision >= 0)
);
