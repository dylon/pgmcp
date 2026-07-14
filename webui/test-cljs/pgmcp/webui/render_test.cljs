(ns pgmcp.webui.render-test
  "Unit coverage for the pure chart geometry (viz), the hiccup transforms
  (render) that back the Markdown, code-highlight, and chart renderers, and the
  new-pane domain normalizers."
  (:require [cljs.test :refer [deftest is testing]]
            [clojure.string :as str]
            [pgmcp.webui.domain :as domain]
            [pgmcp.webui.render :as render]
            [pgmcp.webui.viz :as viz]))

;; ── viz (chart geometry) ────────────────────────────────────────────────

(deftest nice-max-picks-nice-axis-bounds
  (is (= 10 (viz/nice-max 8)))
  (is (= 50 (viz/nice-max 45)))
  (is (= 200 (viz/nice-max 150)))
  (is (= 1 (viz/nice-max 1)))
  (testing "non-positive collapses to 1"
    (is (= 1 (viz/nice-max 0)))
    (is (= 1 (viz/nice-max -5)))))

(deftest linear-scale-maps-domain-to-range
  (is (= 50 ((viz/linear 0 10 0 100) 5)))
  (is (= 0 ((viz/linear 0 10 0 100) 0)))
  (is (= 100 ((viz/linear 0 10 0 100) 10)))
  (testing "zero span collapses to r0"
    (is (= 7 ((viz/linear 3 3 7 99) 3)))))

(deftest line-path-emits-move-then-lines
  (is (= "M0.0 0.0 L10.0 20.0" (viz/line-path [[0 0] [10 20]])))
  (is (= "" (viz/line-path []))))

(deftest series->points-scales-into-box
  (is (= [[5 45] [95 5]] (viz/series->points [0 10] 100 50 5 10)))
  (testing "single point centers on x"
    (is (= [[50 45]] (viz/series->points [0] 100 50 5 10)))))

(deftest bars-stay-inside-the-box
  (let [[b] (viz/bars [10] 100 50 5 10)]
    (is (= 5 (:y b)))
    (is (= 40 (:h b)))
    (is (<= 5 (:x b)))
    (is (pos? (:w b)))))

;; ── render (hiccup transforms) ──────────────────────────────────────────

(deftest class-for-maps-tree-sitter-captures
  (is (= "cm-keyword" (render/class-for "keyword")))
  (is (= "cm-keyword" (render/class-for "keyword.operator")))
  (is (= "cm-string" (render/class-for "string.special")))
  (testing "the sentinel and unknown captures yield no class"
    (is (nil? (render/class-for "none")))
    (is (nil? (render/class-for "totally-unknown")))))

(deftest hast->hiccup-walks-allowed-tags
  (let [tree #js {:type "root"
                  :children #js [#js {:type "element" :tagName "p" :properties #js {}
                                      :children #js [#js {:type "text" :value "hi"}]}]}
        out (render/hast->hiccup tree)
        para (second out)]
    (is (= :<> (first out)))
    (is (= :p (first para)))
    (is (= "hi" (last para)))))

(deftest hast->hiccup-drops-unknown-tags-but-keeps-children
  (let [tree #js {:type "element" :tagName "script" :properties #js {}
                  :children #js [#js {:type "text" :value "x"}]}
        out (render/hast->hiccup tree)]
    (is (= :<> (first out)))
    (is (= "x" (second out)))))

(deftest hast->hiccup-anchors-open-in-new-tab-safely
  (let [tree #js {:type "element" :tagName "a"
                  :properties #js {:href "https://example.com"}
                  :children #js [#js {:type "text" :value "link"}]}
        [tag props] (render/hast->hiccup tree)]
    (is (= :a tag))
    (is (= "https://example.com" (:href props)))
    (is (= "noreferrer" (:rel props)))))

(deftest spans->hiccup-interleaves-text-and-highlighted-spans
  (let [out (render/spans->hiccup "hello" [{:from 0 :to 2 :class "cm-x"}])]
    (is (= :<> (first out)))
    (is (some (fn [n] (and (vector? n) (= :span (first n)) (= "he" (last n))))
              (rest out)))
    (is (some #{"llo"} (rest out))))
  (testing "no spans → the whole text survives"
    (let [out (render/spans->hiccup "abc" [])]
      (is (some #{"abc"} (rest out))))))

;; ── domain (new-pane normalizers) ───────────────────────────────────────

(deftest log-level-status-classifies-levels
  (is (= :danger (domain/log-level-status "ERROR")))
  (is (= :danger (domain/log-level-status "error")))
  (is (= :warn (domain/log-level-status "WARN")))
  (is (= :info (domain/log-level-status "INFO")))
  (is (= :info (domain/log-level-status "trace")))
  (testing "unknown / missing → neutral"
    (is (= :neutral (domain/log-level-status nil)))
    (is (= :neutral (domain/log-level-status "WAT")))))

(deftest normalized-logs-grep-branch-reads-match-spans
  ;; pins the backend grep-span field name (:text, not :matched_text)
  (let [section (first (domain/normalized-logs
                        {:matches [{:line_number 42 :line "boom"
                                    :matched [{:text "boom" :distance 1}]}]
                         :truncated false}))
        row (first (get-in section [:table :rows]))]
    (is (str/includes? (:title section) "grep"))
    (is (= "42" (:line row)))
    (is (= "boom" (:text row)))
    (is (= "boom ~1" (get-in row [:match :chip])))))

(deftest normalized-logs-tail-branch-levels-are-status-chips
  (let [section (first (domain/normalized-logs
                        {:path "/var/log/pgmcp.log"
                         :lines [{:level "ERROR" :ts "t0" :text "kaboom"}]}))
        row (first (get-in section [:table :rows]))]
    (is (str/includes? (:title section) "tail"))
    (is (= "kaboom" (:text row)))
    (is (= :danger (get-in row [:level :status])))))

;; ── WS1: Overview normalizer key alignment (regression pins) ─────────────

(deftest normalized-index-per-project-reads-struct-keys
  (let [sections (domain/normalized-index
                  {:project_count 3 :indexed_file_count 100 :chunk_count 500
                   :per_project [{:project_name "pgmcp" :indexed_file_count 42 :chunk_count 210}]})
        pp (first (filter #(= "Per-project" (:title %)) sections))
        row (first (get-in pp [:table :rows]))]
    (is (= "pgmcp" (:project row)))
    (is (= "42" (:files row)))
    (is (= "210" (:chunks row)))))

(deftest normalized-cron-rollup-job-runs-success-avg-reason
  (let [sections (domain/normalized-cron
                  {:rollup [{:job_name "quality-history" :last_outcome "failed"
                             :run_count 10 :ok_count 8 :avg_ms 1500 :last_error "db timeout"}]})
        jobs (first (filter #(= "Cron jobs" (:title %)) sections))
        row (first (get-in jobs [:table :rows]))]
    (is (= "quality-history" (:job row)))
    (is (= "10" (:runs row)))
    (is (str/includes? (:success row) "80"))
    (is (str/includes? (str (:reason row)) "db timeout"))))

(deftest normalized-cron-recent-reads-job-name-and-reason
  (let [sections (domain/normalized-cron
                  {:recent [{:job_name "db-maintenance" :outcome "skipped"
                             :duration_ms 5 :started_at "t0" :skip_reason "db down"}]})
        recent (first (filter #(= "Recent runs" (:title %)) sections))
        row (first (get-in recent [:table :rows]))]
    (is (= "db-maintenance" (:job row)))
    (is (str/includes? (str (:reason row)) "db down"))))

(deftest normalized-status-surfaces-phase-tile
  (let [sections (domain/normalized-status
                  {:daemon {:version "0.1.0" :phase "ready" :uptime_secs 100}})
        daemon (first sections)]
    (is (some (fn [t] (and (= "Phase" (:label t)) (= "ready" (:value t))))
              (:tiles daemon)))))

;; ── work-item tree (split-pane hierarchy) ───────────────────────────────

(deftest normalized-work-tree-surfaces-id-and-parent
  (let [rows (domain/normalized-work-tree
              {:nodes [{:id 1 :parent_id nil :public_id "PLAN-1" :kind "plan"
                        :status "in_progress" :title "Root" :depth 0}
                       {:id 2 :parent_id 1 :public_id "TASK-1" :kind "task"
                        :status "ready" :title "Child" :depth 1}]})]
    (is (= [1 2] (mapv :id rows)))
    (is (= [nil 1] (mapv :parent-id rows)))
    (is (= "PLAN-1" (:public-id (first rows))))))

(deftest tree-visible-rows-flags-children-and-hides-collapsed
  (let [rows [{:id 1 :parent-id nil :depth 0}
              {:id 2 :parent-id 1 :depth 1}
              {:id 3 :parent-id 2 :depth 2}]]
    (testing "nothing collapsed → all visible; ancestors flagged has-children"
      (let [v (domain/tree-visible-rows rows #{})]
        (is (= [1 2 3] (mapv :id v)))
        (is (= [true true false] (mapv :has-children v)))))
    (testing "collapsing the root hides its whole subtree"
      (is (= [1] (mapv :id (domain/tree-visible-rows rows #{1})))))
    (testing "collapsing a mid node hides only its descendants"
      (is (= [1 2] (mapv :id (domain/tree-visible-rows rows #{2})))))))
