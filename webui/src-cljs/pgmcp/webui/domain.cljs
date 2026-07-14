(ns pgmcp.webui.domain
  (:require [clojure.string :as str]
            [pgmcp.webui.schema :as schema]))

(defn bounded-cons [limit item items]
  (vec (take limit (cons item (or items [])))))

(defn bounded-concat [limit leading trailing]
  (vec (take limit (concat (or leading []) (or trailing [])))))

(defn reject [machine event reason]
  (update-in machine [:s :rings :rejects]
             #(bounded-cons schema/max-rejects
                            {:at (or (:at event) 0)
                             :event (:type event)
                             :reason (or reason "unhandled")}
                            %)))

(defn selected-topics [store]
  (vec (filter #(get-in store [:ui :topics %]) schema/topics)))

(defn all-topics-selected? [store]
  (= (count (selected-topics store)) (count schema/topics)))

(defn topic-selected? [store topic]
  (let [topic (schema/normalize-topic topic)]
    (boolean (get-in store [:ui :topics topic]))))

(defn selectable-topic-change [store topic checked?]
  (let [topic (schema/normalize-topic topic)]
    (if-not (schema/known-topic? topic)
      store
      (let [was-selected? (topic-selected? store topic)
            next-store (cond-> (assoc-in store [:ui :topics topic] checked?)
                         (and was-selected? (not checked?))
                         (assoc-in [:domain :topic-seqs topic]
                                   (get-in store [:domain :applied-seq] 0)))]
        (if (seq (selected-topics next-store))
          next-store
          store)))))

(defn topic-watermark [store topic]
  (let [topic (schema/normalize-topic topic)]
    (get-in store [:domain :topic-seqs topic]
            (get-in store [:domain :applied-seq] 0))))

(defn subscription-since [store]
  (let [selected (selected-topics store)
        fallback (get-in store [:domain :applied-seq] 0)
        watermarks (map #(get-in store [:domain :topic-seqs %] fallback) selected)]
    (if (seq watermarks)
      (apply min watermarks)
      fallback)))

(defn subscription-topics [store]
  (if (all-topics-selected? store)
    []
    (mapv name (selected-topics store))))

(defn subscription [store]
  {:since (subscription-since store)
   :topics (subscription-topics store)})

(defn normalize-event [event]
  (let [topic (schema/normalize-topic (:topic event))]
    (-> event
        (assoc :seq (or (:seq event) 0))
        (assoc :topic topic)
        (update :entity_kind #(or % ""))
        (update :entity_id #(or % ""))
        (update :op #(or % ""))
        (update :payload #(or % {})))))

(defn event-fresh? [store event]
  (let [event (normalize-event event)]
    (> (:seq event) (topic-watermark store (:topic event)))))

(defn push-event [store event]
  (update-in store [:rings :events]
             #(bounded-cons schema/max-events (normalize-event event) %)))

(defn receive-event [store event]
  (let [event (normalize-event event)
        seq (:seq event)
        topic (:topic event)
        known-topic? (schema/known-topic? topic)
        next-store (cond-> (-> store
                               (assoc-in [:domain :applied-seq]
                                         (max (get-in store [:domain :applied-seq] 0) seq))
                               (assoc-in [:domain :server-seq]
                                         (max (get-in store [:domain :server-seq] 0) seq)))
                     known-topic?
                     (assoc-in [:domain :topic-seqs topic]
                               (max (get-in store [:domain :topic-seqs topic] 0) seq)))]
    (if (= :paused (get-in next-store [:control :events]))
      (update-in next-store [:rings :queued-events]
                 #(bounded-cons schema/max-events event %))
      (push-event next-store event))))

(defn frame-server-seq [frame]
  (or (:server_seq frame) (:server-seq frame) 0))

(defn advance-server-seq [store server-seq]
  (assoc-in store [:domain :server-seq]
            (max (get-in store [:domain :server-seq] 0)
                 (get-in store [:domain :applied-seq] 0)
                 (or server-seq 0))))

(defn drain-queued-events [store]
  (let [queued (get-in store [:rings :queued-events])
        events (get-in store [:rings :events])]
    (-> store
        (assoc-in [:rings :events] (bounded-concat schema/max-events queued events))
        (assoc-in [:rings :queued-events] []))))

(defn apply-frame [store frame]
  (let [frame-type (schema/normalize-keyword (:type frame))]
    (cond
      (nil? frame-type)
      store

      (#{:welcome :heartbeat} frame-type)
      (advance-server-seq store (frame-server-seq frame))

      (= :resync frame-type)
      (let [server-seq (frame-server-seq frame)]
        (-> store
            (assoc-in [:control :connection] :error)
            (advance-server-seq server-seq)
            (push-event {:seq server-seq
                         :topic :resync
                         :entity_kind "stream"
                         :entity_id (or (:reason frame) "unknown")
                         :op "snapshot"
                         :payload {}})))

      (and (= :event frame-type)
           (:event frame)
           (event-fresh? store (:event frame)))
      (receive-event store (:event frame))

      :else
      store)))

(defn set-query-field [store field value]
  (let [field (schema/normalize-keyword field)]
    (case field
      :mode (assoc-in store [:ui :query :mode] (schema/normalize-query-mode value))
      :text (assoc-in store [:ui :query :text] (str value))
      :project (assoc-in store [:ui :query :project] (str value))
      :limit (assoc-in store [:ui :query :limit]
                       (str/replace (str value) #"\D" ""))
      store)))

(defn parse-limit
  ([value] (parse-limit value 10))
  ([value default-value]
   (let [parsed (js/parseInt (str value) 10)]
     (if (js/isNaN parsed)
       default-value
       (-> parsed
           (max 1)
           (min 100))))))

(defn request-pending? [store & request-path]
  (let [request-id (get-in store (into [:domain :requests] request-path))]
    (boolean
     (and request-id
          (contains? (get-in store [:domain :requests :pending]) request-id)))))

(defn any-request-pending? [store]
  (boolean (seq (get-in store [:domain :requests :pending]))))

(defn query-runnable? [store]
  (not (str/blank? (get-in store [:ui :query :text]))))

(defn set-mandates-field [store field value]
  (let [field (schema/normalize-keyword field)]
    (case field
      :scope (assoc-in store [:ui :mandates :scope] (schema/normalize-mandate-scope value))
      :project (assoc-in store [:ui :mandates :project] (str value))
      store)))

(defn set-work-field [store field value]
  (let [field (schema/normalize-keyword field)]
    (case field
      :view (assoc-in store [:ui :work :view] (schema/normalize-work-view value))
      :assignee (assoc-in store [:ui :work :assignee] (str value))
      :limit (assoc-in store [:ui :work :limit]
                       (str/replace (str value) #"\D" ""))
      :plan-public-id (assoc-in store [:ui :work :plan-public-id] (str value))
      :kind (assoc-in store [:ui :work :kind] (str value))
      :status (assoc-in store [:ui :work :status] (str value))
      :project (assoc-in store [:ui :work :project] (str value))
      store)))

(defn query-request [store]
  (let [{:keys [mode text project limit]} (get-in store [:ui :query])
        text (str/trim (or text ""))
        project (str/trim (or project ""))
        base {:mode (name (schema/normalize-query-mode mode))
              :limit (parse-limit limit)}
        with-query (if (= :grep (schema/normalize-query-mode mode))
                     (assoc base :pattern text)
                     (assoc base :query text))]
    (cond-> with-query
      (not (str/blank? project)) (assoc :project project))))

(defn mandates-request [store]
  (let [{:keys [scope project]} (get-in store [:ui :mandates])
        project (str/trim (or project ""))]
    (cond-> {:scope (name (schema/normalize-mandate-scope scope))}
      (not (str/blank? project)) (assoc :project project))))

(defn work-request [store]
  (let [{:keys [view assignee limit plan-public-id kind status project]} (get-in store [:ui :work])
        view (schema/normalize-work-view view)
        assignee (str/trim (or assignee ""))
        plan-public-id (str/trim (or plan-public-id ""))
        kind (str/trim (or kind ""))
        status (str/trim (or status ""))
        project (str/trim (or project ""))]
    ;; view=:all omits the view param → the handler browses unconstrained (the
    ;; kind/status/project filters then stand alone rather than intersecting a
    ;; smart-view).
    (cond-> {:limit (parse-limit limit 25)}
      (not= view :all) (assoc :view (name view))
      (not (str/blank? assignee)) (assoc :assignee assignee)
      (and (= :next-actionable view)
           (not (str/blank? plan-public-id))) (assoc :plan_public_id plan-public-id)
      (not (str/blank? kind)) (assoc :kind kind)
      (not (str/blank? status)) (assoc :status status)
      (not (str/blank? project)) (assoc :project project))))

(defn query-results [payload]
  (or (get-in payload [:data :results])
      (:results payload)
      []))

(defn query-truncated? [payload]
  (boolean (or (get-in payload [:data :truncated])
               (:truncated payload))))

(defn row-score [row]
  (or (:similarity row) (:score row) (:rank row) ""))

(defn row-path [row]
  (or (:relative_path row) (:file_path row) (:path row) ""))

(defn row-text [row]
  (or (:chunk row) (:chunk_content row) (:content row) (:snippet row) ""))

(defn row-lines [row]
  (let [start (:start_line row)
        end (:end_line row)]
    (cond
      (and start end (not= start end)) (str start "-" end)
      start (str start)
      :else "")))

(defn fmt-score [score]
  (cond
    (number? score) (.toFixed score 4)
    (str/blank? (str score)) ""
    :else (str score)))

(defn normalized-query-rows [payload]
  (mapv (fn [row]
          {:path (row-path row)
           :lines (row-lines row)
           :language (or (:language row) "")
           :project (or (:project_name row) (:project row) "")
           :score (fmt-score (row-score row))
           :snippet (row-text row)})
        (query-results payload)))

(defn work-items [payload]
  (or (:items payload) []))

(defn normalized-work-rows [payload]
  (mapv (fn [row]
          {:public-id (or (:public_id row) "")
           :kind (or (:kind row) "")
           :status (or (:status row) "")
           :title (or (:title row) "")
           :body (or (:body row) "")
           :priority (str (or (:priority row) 0))
           :claimed-percent (str (or (:claimed_percent row) 0) "%")
           :assignee (or (:assignee row) "")
           :claimed-by (or (:claimed_by row) "")
           :due-at (or (:due_at row) "")
           :severity (or (:severity row) "")
           :parent-id (:parent_id row)
           :root-id (:root_id row)
           :project (or (:project row) (:project_name row) "")})
        (work-items payload)))

;; These work normalizers are grouped with the other work fns (above) but use the
;; shared fmt helpers defined further below; forward-declare so the refs resolve.
(declare present? truncate)

(defn normalized-work-detail
  "Sections for a work-item detail payload {item, timeline, acceptance_criteria,
  bug_details}. The item body is rendered separately (markdown) by the view."
  [payload]
  (let [item (or (:item payload) {})
        bug (:bug_details payload)]
    (vec (remove nil?
      [{:title (str (:public_id item) " — " (or (:title item) ""))
        :kv (vec (remove nil?
              [["Kind" (:kind item)]
               ["Status" (:status item)]
               ["Priority" (str (:priority item))]
               ["Claimed" (str (or (:claimed_percent item) 0) "%")]
               (when (present? (:assignee item)) ["Assignee" (:assignee item)])
               (when (present? (:severity item)) ["Severity" (:severity item)])
               (when (present? (:due_at item)) ["Due" (:due_at item)])
               ["Created" (or (:created_at item) "—")]
               ["Updated" (or (:updated_at item) "—")]]))}
       (when (seq (:acceptance_criteria payload))
         {:title (str "Acceptance criteria (" (count (:acceptance_criteria payload)) ")")
          :table {:columns [{:key :kind :label "Kind"}
                            {:key :desc :label "Description"}
                            {:key :gate :label "Gate"}
                            {:key :req :label "Req"}]
                  :rows (mapv (fn [c] {:kind (:criterion_kind c)
                                       :desc (truncate (:description c) 120)
                                       :gate (or (:gate c) "—")
                                       :req (if (:required c) "yes" "no")})
                              (:acceptance_criteria payload))}})
       (when bug
         {:title "Bug details"
          :kv (vec (remove nil?
                [(when (present? (:reproduction_steps bug)) ["Reproduction" (:reproduction_steps bug)])
                 (when (present? (:expected_behavior bug)) ["Expected" (:expected_behavior bug)])
                 (when (present? (:actual_behavior bug)) ["Actual" (:actual_behavior bug)])
                 (when (present? (:environment bug)) ["Environment" (:environment bug)])
                 (when (present? (:root_cause bug)) ["Root cause" (:root_cause bug)])
                 (when (present? (:resolution bug)) ["Resolution" (:resolution bug)])]))})
       (when (seq (:timeline payload))
         {:title (str "Timeline (" (count (:timeline payload)) ")")
          :table {:columns [{:key :at :label "At"}
                            {:key :kind :label "Event"}
                            {:key :actor :label "Actor"}
                            {:key :summary :label "Summary"}]
                  :rows (mapv (fn [t] {:at (:at t)
                                       :kind (:kind t)
                                       :actor (or (:actor t) "—")
                                       :summary (truncate (:summary t) 120)})
                              (:timeline payload))}})]))))

(defn normalized-work-tree
  "Flatten a tree payload {nodes:[{...item, depth, path}]} into display rows with
  their indentation depth preserved. :id/:parent-id are surfaced so the view can
  compute parent→child visibility for expand/collapse."
  [payload]
  (mapv (fn [n]
          {:id (:id n)
           :parent-id (:parent_id n)
           :public-id (or (:public_id n) "")
           :kind (or (:kind n) "")
           :status (or (:status n) "")
           :title (or (:title n) "")
           :depth (or (:depth n) 0)})
        (or (:nodes payload) [])))

(defn tree-visible-rows
  "Given flat depth-ordered tree rows and a set of collapsed node ids, drop any
  row with a collapsed ancestor and tag each surviving row with :has-children."
  [rows collapsed]
  (let [by-id (into {} (map (juxt :id identity)) rows)
        parents (into #{} (keep :parent-id) rows)
        ancestor-collapsed? (fn [row]
                              (loop [pid (:parent-id row)]
                                (cond
                                  (nil? pid) false
                                  (contains? collapsed pid) true
                                  :else (recur (:parent-id (get by-id pid))))))]
    (into []
          (comp (remove ancestor-collapsed?)
                (map #(assoc % :has-children (contains? parents (:id %)))))
          rows)))

(defn known-event-topic? [event]
  (schema/known-topic? (:topic event)))

(defn visible-event? [store event]
  (or (not (known-event-topic? event))
      (topic-selected? store (:topic event))))

(defn visible-events [store]
  (filterv #(visible-event? store %) (get-in store [:rings :events])))

(defn event-counts [events]
  (reduce (fn [counts event]
            (update counts (schema/topic-name (:topic event)) (fnil inc 0)))
          {}
          events))

(defn event-summary [store]
  (let [events (visible-events store)]
    {:applied-seq (get-in store [:domain :applied-seq] 0)
     :server-seq (get-in store [:domain :server-seq] 0)
     :visible-count (count events)
     :queued-count (count (get-in store [:rings :queued-events]))
     :topic-counts (event-counts events)}))

(defn mandate-source-row [source]
  (assoc source :row-kind :source))

(defn project-override-row [override-facts]
  (when override-facts
    {:row-kind :project-override
     :scope "project"
     :kind "pgmcp_project_override"
     :path (:source_path override-facts)
     :text (:text override-facts)
     :truncated (:truncated override-facts)
     :size_bytes (:size_bytes override-facts)
     :sha256 (:sha256 override-facts)}))

(defn skipped-source-row [source]
  {:row-kind :skipped
   :scope (:scope source)
   :kind (:kind source)
   :path (:path source)
   :text (:reason source)})

(defn mandate-sources [payload]
  (let [mandates (:mandates payload)]
    (vec
     (concat
      (map mandate-source-row (or (:sources mandates) []))
      (keep identity [(project-override-row (:project_override mandates))])
      (map skipped-source-row (or (:skipped_sources mandates) []))))))

;; ── Formatting helpers (pure; js/Math + .toFixed/.toLocaleString are already
;;    used elsewhere in this namespace, so they stay within the gate's purity
;;    rules for schema/model/domain/machine). ────────────────────────────────

(defn present? [v]
  (and (some? v) (not= v "")))

(defn fmt-int [n]
  (if (number? n) (.toLocaleString (js/Math.round n)) (str (or n ""))))

(defn fmt-bytes [n]
  (if (number? n)
    (loop [v (double (js/Math.abs n))
           us ["B" "KiB" "MiB" "GiB" "TiB" "PiB"]]
      (if (or (< v 1024.0) (nil? (second us)))
        (str (.toFixed v (cond (< v 10) 2 (< v 100) 1 :else 0)) " " (first us))
        (recur (/ v 1024.0) (rest us))))
    (str (or n ""))))

(defn fmt-duration-ms [ms]
  (if (number? ms)
    (cond
      (< ms 1000) (str (js/Math.round ms) " ms")
      (< ms 60000) (str (.toFixed (/ ms 1000.0) 1) " s")
      (< ms 3600000) (str (.toFixed (/ ms 60000.0) 1) " min")
      :else (str (.toFixed (/ ms 3600000.0) 1) " h"))
    (str (or ms ""))))

(defn fmt-secs [s]
  (if (number? s) (fmt-duration-ms (* s 1000)) (str (or s ""))))

(defn fmt-pct
  "A value already in percent units (0..100)."
  [x]
  (if (number? x) (str (.toFixed x 1) "%") (str (or x ""))))

(defn safe-frac [n d]
  (if (and (number? n) (number? d) (pos? d)) (/ n d) 0))

(defn truncate [s n]
  (let [s (str s)]
    (if (> (count s) n) (str (subs s 0 n) "…") s)))

(defn threshold-status [frac]
  (cond (>= frac 0.9) :danger (>= frac 0.7) :warn :else :ok))

(defn humanize-key [k]
  (str/replace (name k) "_" " "))

;; ── Overview stats normalizers. Each returns a vector of widget `section`
;;    maps (see pgmcp.webui.views.widgets); nils are dropped by `sections`.
;;    All access is defensive so a partial/absent shape renders what it has. ──

(defn normalized-status [data]
  (let [d (or (:daemon data) {})
        db (or (:database data) {})
        emb (or (:embeddings data) {})
        pools (or (:pools data) {})
        gp (or (:general pools) {})]
    [(when (or (seq d) (present? (:phase data)))
       {:title "Daemon"
        :tiles (vec (remove nil?
                     [(when (present? (:version d)) {:label "Version" :value (:version d)})
                      (when (present? (or (:phase d) (:phase data)))
                        {:label "Phase" :value (or (:phase d) (:phase data)) :status :ok})
                      (when (number? (:uptime_secs d)) {:label "Uptime" :value (fmt-secs (:uptime_secs d))})
                      (when (number? (:current_rss_bytes d))
                        {:label "RSS" :value (fmt-bytes (:current_rss_bytes d))
                         :sub (when (number? (:peak_rss_bytes d)) (str "peak " (fmt-bytes (:peak_rss_bytes d))))})
                      (when (some? (:http_mcp_sessions d)) {:label "MCP sessions" :value (fmt-int (:http_mcp_sessions d))})
                      (when (some? (:heavy_cron_running d))
                        {:label "Heavy cron" :value (if (:heavy_cron_running d) "running" "idle")
                         :status (if (:heavy_cron_running d) :warn :ok)})]))})
     (when (seq db)
       {:title "Database"
        :kv (vec (remove nil?
                  [(when (present? (:url db)) ["url" (:url db)])
                   (when (present? (:name db)) ["database" (:name db)])
                   (when (present? (:server_version db)) ["server" (:server_version db)])
                   (when (present? (:vector_extension_version db)) ["pgvector" (:vector_extension_version db)])
                   (when (some? (:pool_size db))
                     ["pool" (str (:pool_active db) " active / " (:pool_idle db) " idle / " (:pool_size db) " size")])]))})
     (when (seq emb)
       {:title "Embeddings"
        :kv (vec (remove nil?
                  [(when (present? (:model emb)) ["model" (:model emb)])
                   (when (some? (:dimensions emb)) ["dimensions" (fmt-int (:dimensions emb))])
                   (when (present? (:backend emb)) ["backend" (:backend emb)])
                   (when (present? (:device emb)) ["device" (:device emb)])
                   (when (some? (:max_length emb)) ["max length" (fmt-int (:max_length emb))])
                   (when (some? (:inference_batch_size emb)) ["batch size" (fmt-int (:inference_batch_size emb))])]))})
     (when (and (seq gp) (number? (:max_threads gp)))
       (let [frac (safe-frac (:active_workers gp) (:max_threads gp))]
         {:title "Worker pool"
          :meters [{:name "general"
                    :fraction frac
                    :status (threshold-status frac)
                    :label (str (:active_workers gp) " / " (:max_threads gp) " workers · queue " (or (:queue_depth gp) 0))}]}))]))

(defn normalized-index [data]
  [{:title "Index"
    :tiles (vec (remove nil?
                 [(when (some? (:project_count data)) {:label "Projects" :value (fmt-int (:project_count data))})
                  (when (some? (:indexed_file_count data)) {:label "Files" :value (fmt-int (:indexed_file_count data))})
                  (when (some? (:chunk_count data)) {:label "Chunks" :value (fmt-int (:chunk_count data))})
                  (when (present? (:last_indexed_at data)) {:label "Last indexed" :value (:last_indexed_at data)})]))}
   (when (seq (:per_project data))
     {:title "Per-project"
      :table {:columns [{:key :project :label "Project"}
                        {:key :files :label "Files" :align :num}
                        {:key :chunks :label "Chunks" :align :num}]
              :rows (mapv (fn [r]
                            {:project (or (:project_name r) (:project r) (:name r) "")
                             :files (fmt-int (or (:indexed_file_count r) (:file_count r) (:files r)))
                             :chunks (fmt-int (or (:chunk_count r) (:chunks r)))})
                          (:per_project data))}})
   (when (seq (:failure_kind_counts data))
     {:title "Index failures"
      :chips (mapv (fn [[k v]] {:label (str (name k) " · " v) :status :warn})
                   (:failure_kind_counts data))})])

(defn status-chip [value ok-set danger-set]
  {:chip (or value "—")
   :status (cond (contains? ok-set value) :ok
                 (contains? danger-set value) :danger
                 :else :neutral)})

(defn normalized-cron [data]
  [(when (seq (:rollup data))
     {:title "Cron jobs"
      :table {:columns [{:key :job :label "Job"}
                        {:key :last :label "Last"}
                        {:key :success :label "Success" :align :num}
                        {:key :avg :label "Avg" :align :num}
                        {:key :runs :label "Runs" :align :num}
                        {:key :reason :label "Reason"}]
              :rows (mapv (fn [r]
                            (let [runs (or (:run_count r) (:runs r) (:total_runs r))
                                  oks (:ok_count r)
                                  reason (or (:last_error r) (:last_skip_reason r))]
                              {:job (or (:job_name r) (:job r))
                               :last (status-chip (or (:last_status r) (:last_outcome r)) #{"ok"} #{"failed" "panicked"})
                               :success (cond
                                          (and (number? oks) (number? runs) (pos? runs))
                                          (fmt-pct (* 100 (safe-frac oks runs)))
                                          (number? (:success_rate r)) (fmt-pct (:success_rate r))
                                          :else "—")
                               :avg (fmt-duration-ms (or (:avg_ms r) (:avg_duration_ms r)))
                               :runs (fmt-int runs)
                               :reason (when (present? reason) (truncate reason 100))}))
                          (:rollup data))}})
   (when (seq (:recent data))
     {:title "Recent runs"
      :table {:columns [{:key :job :label "Job"}
                        {:key :outcome :label "Outcome"}
                        {:key :duration :label "Duration" :align :num}
                        {:key :at :label "At"}
                        {:key :reason :label "Reason"}]
              :rows (mapv (fn [r]
                            (let [reason (or (:skip_reason r) (:error_detail r))]
                              {:job (or (:job_name r) (:job r))
                               :outcome (status-chip (:outcome r) #{"ok"} #{"failed" "panicked"})
                               :duration (fmt-duration-ms (:duration_ms r))
                               :at (or (:at r) (:started_at r) (:created_at r) "—")
                               :reason (when (present? reason) (truncate reason 100))}))
                          (:recent data))}})])

(defn normalized-clients [data]
  [(when (seq (:active data))
     {:title "Active clients"
      :table {:columns [{:key :client :label "Client"}
                        {:key :project :label "Project"}
                        {:key :cwd :label "cwd"}
                        {:key :pid :label "PID" :align :num}
                        {:key :idle :label "Idle" :align :num}
                        {:key :alive :label "Alive"}]
              :rows (mapv (fn [r]
                            {:client (or (:client_name r) "")
                             :project (or (:project r) "")
                             :cwd (or (:cwd r) "")
                             :pid (str (or (:pid r) ""))
                             :idle (fmt-secs (:idle_secs r))
                             :alive {:chip (if (:alive r) "alive" "gone")
                                     :status (if (:alive r) :ok :neutral)}})
                          (:active data))}})
   (when (seq (:project_matrix data))
     {:title "Client × project activity"
      :table {:columns [{:key :client :label "Client"}
                        {:key :project :label "Project"}
                        {:key :edits :label "Edits" :align :num}
                        {:key :reads :label "Reads" :align :num}
                        {:key :last :label "Last activity"}]
              :rows (mapv (fn [r]
                            {:client (or (:client_name r) "")
                             :project (or (:project r) "")
                             :edits (fmt-int (:edit_count r))
                             :reads (fmt-int (:read_count r))
                             :last (or (:last_activity r) (:last_edit r) "—")})
                          (:project_matrix data))}})])

(defn normalized-telemetry [data]
  [{:title "MCP tool telemetry"
    :table {:columns [{:key :tool :label "Tool"}
                      {:key :calls :label "Calls" :align :num}
                      {:key :errors :label "Errors" :align :num}
                      {:key :avg :label "Avg" :align :num}
                      {:key :max :label "Max" :align :num}
                      {:key :last :label "Last"}]
            :rows (mapv (fn [r]
                          (let [errs (or (:error_count r) 0)]
                            {:tool (:tool r)
                             :calls (fmt-int (:calls r))
                             :errors {:chip (str errs) :status (if (pos? errs) :danger :ok)}
                             :avg (fmt-duration-ms (:avg_duration_ms r))
                             :max (fmt-duration-ms (:max_duration_ms r))
                             :last (or (:last_ts r) "—")}))
                        (or (:tools data) []))
            :empty-text "No tool calls in the telemetry window."}}])

(def counter-highlights
  [:uptime_secs :mcp_requests :mcp_errors :files_indexed :chunks_embedded
   :bytes_processed :active_work_pool_threads :work_pool_queue_depth :embed_errors])

(defn normalized-counters [data]
  (let [tiles (vec (for [k counter-highlights
                         :let [v (get data k)]
                         :when (number? v)]
                     {:label (humanize-key k)
                      :value (cond
                               (= k :uptime_secs) (fmt-secs v)
                               (= k :bytes_processed) (fmt-bytes v)
                               :else (fmt-int v))
                      :status (when (and (contains? #{:mcp_errors :embed_errors} k) (pos? v)) :warn)}))
        entries (sort-by (comp name key) (seq (or data {})))]
    [(when (seq tiles) {:title "Highlights" :tiles tiles})
     {:title "All counters"
      :kv (vec (for [[k v] entries]
                 [(humanize-key k) (if (number? v) (fmt-int v) (str v))]))}]))

(defn cpu-core-status [pct]
  (cond (>= pct 90) :danger (>= pct 70) :warn :else :ok))

(defn normalized-resources
  "Shape the /api/resources payload into widget sections: per-core CPU meters,
  memory meters/tiles, GPU cards, and pgmcp worker-pool tiles. Defensive — a
  partial/absent shape renders what it has."
  [payload]
  (let [sys (or (:system payload) {})
        cpu (or (:cpu sys) {})
        mem (or (:memory sys) {})
        proc (or (:process sys) {})
        pools (or (:worker_pools payload) {})
        cores (:per_core_pct cpu)]
    [(when (seq cores)
       {:title (str "CPU · " (fmt-pct (:aggregate_pct cpu)) " avg over " (:core_count cpu) " cores")
        :meters (vec (map-indexed
                      (fn [idx pct]
                        {:name (str "cpu" idx)
                         :fraction (/ (or pct 0) 100.0)
                         :status (cpu-core-status (or pct 0))
                         :label (fmt-pct pct)})
                      cores))})
     (when (or (seq cores) (present? (:load1 cpu)))
       {:title "Load average"
        :tiles [{:label "1 min" :value (str (:load1 cpu))}
                {:label "5 min" :value (str (:load5 cpu))}
                {:label "15 min" :value (str (:load15 cpu))}]})
     (when (number? (:total_bytes mem))
       {:title "Memory"
        :meters (vec (remove nil?
                      [{:name "RAM" :fraction (/ (or (:used_pct mem) 0) 100.0)
                        :status (threshold-status (/ (or (:used_pct mem) 0) 100.0))
                        :label (str (fmt-bytes (:used_bytes mem)) " / " (fmt-bytes (:total_bytes mem)))}
                       (when (pos? (or (:swap_total_bytes mem) 0))
                         {:name "swap"
                          :fraction (safe-frac (:swap_used_bytes mem) (:swap_total_bytes mem))
                          :status (threshold-status (safe-frac (:swap_used_bytes mem) (:swap_total_bytes mem)))
                          :label (str (fmt-bytes (:swap_used_bytes mem)) " / " (fmt-bytes (:swap_total_bytes mem)))})]))})
     (when (seq (:gpu sys))
       {:title "GPU"
        :tiles (mapv (fn [g]
                       {:label (str "GPU" (:index g) " · " (:name g))
                        :value (str (:util_pct g) "% util")
                        :sub (str (fmt-bytes (:mem_used_bytes g)) " / " (fmt-bytes (:mem_total_bytes g))
                                  " · " (:temperature_c g) "°C · "
                                  (.toFixed (or (:power_watts g) 0) 0) "W")
                        :status (threshold-status (/ (or (:util_pct g) 0) 100.0))})
                     (:gpu sys))})
     {:title "pgmcp process & worker pools"
      :tiles (vec (remove nil?
                   [(when (number? (:rss_bytes proc))
                      {:label "RSS" :value (fmt-bytes (:rss_bytes proc))
                       :sub (str "peak " (fmt-bytes (:peak_rss_bytes proc)))})
                    (when (number? (:threads proc)) {:label "OS threads" :value (fmt-int (:threads proc))})
                    (when (some? (:active_work_pool_threads pools))
                      {:label "Work-pool threads" :value (fmt-int (:active_work_pool_threads pools))})
                    (when (some? (:work_pool_queue_depth pools))
                      {:label "Queue depth" :value (fmt-int (:work_pool_queue_depth pools))
                       :status (when (pos? (or (:work_pool_queue_depth pools) 0)) :warn)})
                    (when (some? (:embed_workers_alive pools))
                      {:label "Embed workers" :value (fmt-int (:embed_workers_alive pools))})
                    (when (some? (:db_pool_active pools))
                      {:label "DB pool" :value (str (:db_pool_active pools) " / " (:db_pool_size pools))
                       :sub (str (:db_pool_idle pools) " idle")})
                    (when (some? (:uptime_secs payload))
                      {:label "Uptime" :value (fmt-secs (:uptime_secs payload))})]))}]))

(defn normalized-metrics [payload]
  (let [series (or (:series payload) "tool_calls")
        buckets (or (:buckets payload) [])
        first-b (first buckets)
        numeric-keys (when first-b (filter #(number? (get first-b %)) (keys first-b)))
        primary (case series "tool_calls" :calls "cron" :runs (first numeric-keys))
        values (mapv #(get % primary) buckets)
        cols (if first-b (cons "ts" (sort (map name (remove #{:ts} (keys first-b))))) [])]
    [{:title (str "Metrics · " series " (" (count buckets) " buckets)")
      :chart {:type :bar
              :values values
              :caption (str (some-> primary name) " per " (or (:bucket payload) "bucket"))}}
     (when (seq buckets)
       {:title "Series data"
        :table {:columns (mapv (fn [c] {:key (keyword c) :label c :align (when (not= c "ts") :num)}) cols)
                :rows (mapv (fn [b]
                              (reduce (fn [acc c]
                                        (let [k (keyword c) v (get b k)]
                                          (assoc acc k (if (and (number? v) (not= c "ts")) (fmt-int v) (str v)))))
                                      {} cols))
                            buckets)}})]))

(defn normalized-clients-panel [payload]
  (vec (normalized-clients (:data payload))))

(defn normalized-database [payload]
  (let [cols (or (:columns payload) [])
        rows (or (:rows payload) [])]
    [{:title (str (or (:table payload) "table") " · " (or (:total payload) 0) " rows"
                  " (showing " (or (:offset payload) 0) "–" (+ (or (:offset payload) 0) (count rows)) ")")
      :table {:columns (mapv (fn [c] {:key (keyword (:name c)) :label (:name c)
                                      :align (when (contains? #{"int" "float"} (:type c)) :num)})
                             cols)
              :rows (mapv (fn [r] (reduce (fn [acc c]
                                            (let [k (keyword (:name c))]
                                              (assoc acc k (str (get r k)))))
                                          {} cols))
                          rows)
              :empty-text "No rows."}}]))

(defn log-level-status [level]
  (case (some-> level str/upper-case)
    "ERROR" :danger
    "WARN" :warn
    ("INFO" "DEBUG" "TRACE") :info
    :neutral))

(defn normalized-logs [payload]
  (if (contains? payload :matches)
    [{:title (str "Log grep · " (count (:matches payload)) " matches"
                  (when (:truncated payload) " (truncated)"))
      :table {:columns [{:key :line :label "#" :align :num}
                        {:key :text :label "Line"}
                        {:key :match :label "Match"}]
              :rows (mapv (fn [m]
                            (let [hit (first (:matched m))]
                              {:line (str (:line_number m))
                               :text (:line m)
                               :match {:chip (str (:text hit)
                                                  (when-let [d (:distance hit)] (str " ~" d)))
                                       :status :info}}))
                          (:matches payload))
              :empty-text "No matches."}}]
    [{:title (str "Log tail"
                  (when (present? (:path payload)) (str " · " (:path payload)))
                  (when (:truncated payload) " (truncated)"))
      :table {:columns [{:key :level :label "Level"}
                        {:key :ts :label "Time"}
                        {:key :text :label "Message"}]
              :rows (mapv (fn [l]
                            {:level (when (present? (:level l))
                                      {:chip (:level l) :status (log-level-status (:level l))})
                             :ts (or (:ts l) "")
                             :text (:text l)})
                          (:lines payload))
              :empty-text "No log lines."}}]))

(defn normalized-experiment-detail [payload]
  (let [x (or (:experiment payload) {})]
    (vec (remove nil?
      [{:title (str (or (:title x) (:slug x) "experiment"))
        :kv (vec (remove nil?
              [["Slug" (:slug x)]
               ["Kind" (:kind x)]
               ["Status" (:status x)]
               ["Project" (or (:project x) "—")]
               (when (present? (:question x)) ["Question" (:question x)])
               (when (present? (:git_ref x)) ["Git ref" (:git_ref x)])
               (when (present? (:plan_ref x)) ["Plan ref" (:plan_ref x)])
               ["Created" (or (:created_at x) "—")]
               ["Updated" (or (:updated_at x) "—")]]))}
       (when (seq (:hypotheses payload))
         {:title (str "Hypotheses (" (count (:hypotheses payload)) ")")
          :table {:columns [{:key :statement :label "Statement"}
                            {:key :metric :label "Metric"}
                            {:key :verdict :label "Verdict"}]
                  :rows (mapv (fn [h] {:statement (truncate (:statement h) 140)
                                       :metric (or (:primary_metric h) "—")
                                       :verdict (or (:verdict h) "—")})
                              (:hypotheses payload))}})
       (when (seq (:measurements payload))
         {:title (str "Measurements (" (count (:measurements payload)) " runs)")
          :table {:columns [{:key :run :label "Run"}
                            {:key :arm :label "Arm"}
                            {:key :status :label "Status"}
                            {:key :n :label "n" :align :num}]
                  :rows (mapv (fn [m] {:run (:run_id m)
                                       :arm (or (:arm_label m) "—")
                                       :status (:status m)
                                       :n (fmt-int (:sample_count m))})
                              (:measurements payload))}})
       (when (seq (:decisions payload))
         {:title (str "Decisions (" (count (:decisions payload)) ")")
          :table {:columns [{:key :test :label "Test"}
                            {:key :metric :label "Metric"}
                            {:key :p :label "p" :align :num}
                            {:key :effect :label "Effect" :align :num}
                            {:key :verdict :label "Verdict"}]
                  :rows (mapv (fn [d] {:test (or (:test_type d) "—")
                                       :metric (or (:metric d) "—")
                                       :p (:p_value d)
                                       :effect (:effect_size d)
                                       :verdict (or (:verdict d) "—")})
                              (:decisions payload))}})
       (when (seq (:artifacts payload))
         {:title (str "Artifacts (" (count (:artifacts payload)) ")")
          :table {:columns [{:key :label :label "Label"}
                            {:key :kind :label "Kind"}
                            {:key :tool :label "Tool"}]
                  :rows (mapv (fn [a] {:label (or (:label a) "—")
                                       :kind (or (:kind a) "—")
                                       :tool (or (:tool a) "—")})
                              (:artifacts payload))}})
       (when (seq (:timeline payload))
         {:title "Timeline"
          :table {:columns [{:key :at :label "At"}
                            {:key :event :label "Event"}
                            {:key :detail :label "Detail"}]
                  :rows (mapv (fn [e] {:at (:at e)
                                       :event (:event e)
                                       :detail (when (present? (:detail e)) (truncate (:detail e) 100))})
                              (:timeline payload))}})]))))

(defn normalized-stats [kind payload]
  (let [data (or (:data payload) {})]
    (case (schema/normalize-stats-kind kind)
      :status (normalized-status data)
      :index (normalized-index data)
      :cron (normalized-cron data)
      :clients (normalized-clients data)
      :telemetry (normalized-telemetry data)
      :counters (normalized-counters data)
      [{:title (name kind) :kv (mapv (fn [[k v]] [(humanize-key k) (str v)]) (seq data))}])))
