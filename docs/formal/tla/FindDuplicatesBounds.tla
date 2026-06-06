----------------------------- MODULE FindDuplicatesBounds -----------------------------
(***************************************************************************)
(* `find_duplicates` request/filter model.  Clustering is pure union-find;  *)
(* this spec checks pgmcp's adapter bounds and cross-language row filters.  *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

MaxLimit == 3
FetchMultiplier == 5
MaxFetch == MaxLimit * FetchMultiplier
Projects == {"p1", "p2", "p3"}
Languages == {"rust", "python", "go"}

Rows ==
    { [id |-> 1, similarity |-> 90, lang_a |-> "rust", lang_b |-> "python",
       project_ok |-> TRUE, same_repo |-> FALSE],
      [id |-> 2, similarity |-> 99, lang_a |-> "rust", lang_b |-> "python",
       project_ok |-> FALSE, same_repo |-> FALSE],
      [id |-> 3, similarity |-> 95, lang_a |-> "go", lang_b |-> "python",
       project_ok |-> TRUE, same_repo |-> TRUE],
      [id |-> 4, similarity |-> 50, lang_a |-> "rust", lang_b |-> "go",
       project_ok |-> TRUE, same_repo |-> FALSE] }

Requests ==
    { [id |-> 1, sim_finite |-> TRUE, raw_similarity |-> 90,
       raw_min_projects |-> 2, raw_limit |-> 2, language |-> "none",
       include_same_repo |-> FALSE],
      [id |-> 2, sim_finite |-> TRUE, raw_similarity |-> 0 - 10,
       raw_min_projects |-> 0, raw_limit |-> 0 - 1, language |-> "rust",
       include_same_repo |-> FALSE],
      [id |-> 3, sim_finite |-> TRUE, raw_similarity |-> 120,
       raw_min_projects |-> 2, raw_limit |-> 99, language |-> "python",
       include_same_repo |-> TRUE],
      [id |-> 4, sim_finite |-> FALSE, raw_similarity |-> 90,
       raw_min_projects |-> 2, raw_limit |-> 1, language |-> "none",
       include_same_repo |-> FALSE] }

RequestIds == {r.id : r \in Requests}

ResponseRecord ==
    [ request_id: RequestIds,
      accepted: BOOLEAN,
      min_similarity: 0..100,
      min_projects: 1..128,
      limit: 1..MaxLimit,
      fetch_limit: 1..MaxFetch,
      embedding_reported: 0..MaxLimit,
      cross_reported: SUBSET {row.id : row \in Rows} ]

VARIABLES responses, seen, dbState

vars == <<responses, seen, dbState>>

Init ==
    /\ responses = <<>>
    /\ seen = {}
    /\ dbState = "unchanged"

ClampSimilarity(v) ==
    IF v < 0 THEN 0 ELSE IF v > 100 THEN 100 ELSE v

ClampLimit(v) ==
    IF v < 1 THEN 1 ELSE IF v > MaxLimit THEN MaxLimit ELSE v

ClampMinProjects(v) ==
    IF v < 1 THEN 1 ELSE IF v > 128 THEN 128 ELSE v

LanguageMatches(row, lang) ==
    \/ lang = "none"
    \/ row.lang_a = lang
    \/ row.lang_b = lang

CrossRowAllowed(row, r) ==
    /\ row.project_ok
    /\ row.similarity >= ClampSimilarity(r.raw_similarity)
    /\ LanguageMatches(row, r.language)
    /\ (r.include_same_repo \/ ~row.same_repo)

AllowedCrossIds(r) == {row.id : row \in {x \in Rows : CrossRowAllowed(x, r)}}

Process(r) ==
    /\ r \in Requests
    /\ r.id \notin seen
    /\ seen' = seen \cup {r.id}
    /\ dbState' = dbState
    /\ IF ~r.sim_finite THEN
          responses' = Append(responses,
              [request_id |-> r.id,
               accepted |-> FALSE,
               min_similarity |-> 0,
               min_projects |-> 1,
               limit |-> 1,
               fetch_limit |-> 1,
               embedding_reported |-> 0,
               cross_reported |-> {}])
       ELSE
          LET lim == ClampLimit(r.raw_limit) IN
          responses' = Append(responses,
              [request_id |-> r.id,
               accepted |-> TRUE,
               min_similarity |-> ClampSimilarity(r.raw_similarity),
               min_projects |-> ClampMinProjects(r.raw_min_projects),
               limit |-> lim,
               fetch_limit |-> lim * FetchMultiplier,
               embedding_reported |-> lim,
               cross_reported |-> AllowedCrossIds(r)])

Next == \E r \in Requests : Process(r)

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ responses \in Seq(ResponseRecord)
    /\ seen \subseteq RequestIds
    /\ dbState = "unchanged"

FiniteSimilarityRequired ==
    \A i \in 1..Len(responses) :
        LET r == CHOOSE x \in Requests : x.id = responses[i].request_id IN
        ~r.sim_finite => responses[i].accepted = FALSE

LimitsBoundFetchAndOutput ==
    \A i \in 1..Len(responses) :
        /\ responses[i].limit <= MaxLimit
        /\ responses[i].fetch_limit <= MaxFetch
        /\ responses[i].embedding_reported <= responses[i].limit

SimilarityClamped ==
    \A i \in 1..Len(responses) :
        LET r == CHOOSE x \in Requests : x.id = responses[i].request_id IN
        responses[i].accepted =>
            responses[i].min_similarity = ClampSimilarity(r.raw_similarity)

CrossLanguageRowsAreScoped ==
    \A i \in 1..Len(responses) :
        LET r == CHOOSE x \in Requests : x.id = responses[i].request_id IN
        \A row \in Rows :
            row.id \in responses[i].cross_reported => CrossRowAllowed(row, r)

ReadOnlyAdapter ==
    dbState = "unchanged"

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        FiniteSimilarityRequired /\
        LimitsBoundFetchAndOutput /\
        SimilarityClamped /\
        CrossLanguageRowsAreScoped /\
        ReadOnlyAdapter)

================================================================================
