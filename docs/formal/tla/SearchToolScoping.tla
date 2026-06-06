---- MODULE SearchToolScoping ----
(*
 * Search, read, orient, commit-search, and telemetry scoping for pgmcp
 * MCP tools.
 *
 * The model abstracts over ranking and storage layout. It checks the safety
 * boundary every search-like tool must preserve:
 *
 *   - optional project filters only remove rows;
 *   - file-search tools never return commit rows;
 *   - orient project snapshots expose only file rows from their named project;
 *   - commit-search never returns file rows;
 *   - read_file returns only the indexed row for the requested path;
 *   - telemetry records a project only when one was explicit in the request.
 *
 * Code loci:
 *   src/db/queries/search.rs
 *   src/mcp/tools/tool_text_search.rs
 *   src/mcp/tools/tool_hybrid_search.rs
 *   src/mcp/tools/tool_read_file.rs
 *   src/mcp/tools/tool_search_commits.rs
 *   src/mcp/server.rs
 *)
EXTENDS Naturals, FiniteSets

CONSTANTS
    Projects,
    NoProject,
    NoPath,
    NoTelemetryProject

ProjectFilter == Projects \cup {NoProject}

FileRows == {
    [id |-> "file-a", project |-> "project-a", path |-> "/repo-a/src/lib.rs"],
    [id |-> "file-b", project |-> "project-b", path |-> "/repo-b/src/lib.rs"],
    [id |-> "file-unscoped", project |-> NoProject, path |-> "/unscoped/no-project.rs"]
}

CommitRows == {
    [id |-> "commit-a", project |-> "project-a"],
    [id |-> "commit-b", project |-> "project-b"]
}

PathValues == {NoPath, "/repo-a/src/lib.rs", "/repo-b/src/lib.rs", "/missing.rs"}

FileSearchKinds == {"semantic_search", "text_search", "grep", "hybrid_search"}
OrientKinds == {"orient"}
CommitSearchKinds == {"search_commits"}
ReadKinds == {"read_file"}
ToolKinds == FileSearchKinds \cup OrientKinds \cup CommitSearchKinds \cup ReadKinds

Requests ==
    { [kind |-> kind, project |-> project, path |-> NoPath] :
        kind \in FileSearchKinds \cup CommitSearchKinds,
        project \in ProjectFilter }
    \cup
    { [kind |-> "orient", project |-> project, path |-> NoPath] :
        project \in Projects }
    \cup
    { [kind |-> "read_file", project |-> NoProject, path |-> path] :
        path \in PathValues \ {NoPath} }

VARIABLES
    request,
    file_results,
    commit_results,
    telemetry_project

vars == <<request, file_results, commit_results, telemetry_project>>

ProjectAllowed(project, rowProject) ==
    \/ project = NoProject
    \/ rowProject = project

CandidateFileRows(req) ==
    IF req.kind = "read_file" THEN
        { row \in FileRows :
            /\ row.path = req.path
            /\ ProjectAllowed(req.project, row.project) }
    ELSE IF req.kind \in FileSearchKinds \cup OrientKinds THEN
        { row \in FileRows : ProjectAllowed(req.project, row.project) }
    ELSE
        {}

CandidateCommitRows(req) ==
    IF req.kind \in CommitSearchKinds THEN
        { row \in CommitRows : ProjectAllowed(req.project, row.project) }
    ELSE
        {}

ExpectedTelemetryProject(req) ==
    IF req.project = NoProject THEN NoTelemetryProject ELSE req.project

ResponseShape(req, files, commits, telemetry) ==
    /\ files \subseteq CandidateFileRows(req)
    /\ commits \subseteq CandidateCommitRows(req)
    /\ req.kind \in FileSearchKinds \cup OrientKinds => commits = {}
    /\ req.kind \in CommitSearchKinds => files = {}
    /\ req.kind = "read_file" =>
        /\ commits = {}
        /\ files = CandidateFileRows(req)
        /\ Cardinality(files) <= 1
    /\ telemetry = ExpectedTelemetryProject(req)

Init ==
    /\ request \in Requests
    /\ file_results \in SUBSET FileRows
    /\ commit_results \in SUBSET CommitRows
    /\ telemetry_project \in Projects \cup {NoTelemetryProject}
    /\ ResponseShape(request, file_results, commit_results, telemetry_project)

Next ==
    /\ request' \in Requests
    /\ file_results' \in SUBSET FileRows
    /\ commit_results' \in SUBSET CommitRows
    /\ telemetry_project' \in Projects \cup {NoTelemetryProject}
    /\ ResponseShape(request', file_results', commit_results', telemetry_project')

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ request \in Requests
    /\ file_results \subseteq FileRows
    /\ commit_results \subseteq CommitRows
    /\ telemetry_project \in Projects \cup {NoTelemetryProject}

NoCrossProjectFileResults ==
    \A row \in file_results :
        ProjectAllowed(request.project, row.project)

NoCrossProjectCommitResults ==
    \A row \in commit_results :
        ProjectAllowed(request.project, row.project)

ReadFileExactPath ==
    request.kind = "read_file" =>
        /\ Cardinality(file_results) <= 1
        /\ \A row \in file_results : row.path = request.path

NoWrongResultKind ==
    /\ request.kind \in FileSearchKinds \cup OrientKinds => commit_results = {}
    /\ request.kind \in CommitSearchKinds => file_results = {}
    /\ request.kind = "read_file" => commit_results = {}

OrientRequiresProject ==
    request.kind = "orient" => request.project \in Projects

TelemetryDoesNotInventProject ==
    /\ request.project = NoProject => telemetry_project = NoTelemetryProject
    /\ request.project # NoProject => telemetry_project = request.project

Invariants ==
    /\ TypeOK
    /\ NoCrossProjectFileResults
    /\ NoCrossProjectCommitResults
    /\ ReadFileExactPath
    /\ NoWrongResultKind
    /\ OrientRequiresProject
    /\ TelemetryDoesNotInventProject

====
