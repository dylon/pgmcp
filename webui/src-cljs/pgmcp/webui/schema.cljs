(ns pgmcp.webui.schema
  (:require [clojure.string :as str]))

(def max-events 200)
(def max-rejects 64)
(def max-preview-chars 2048)

(def topics
  [:tracker :mandate :cron :task :index :client :scanner :control :trace :status])

(def topic-labels
  {:tracker "Tracker"
   :mandate "Mandate"
   :cron "Cron"
   :task "Task"
   :index "Index"
   :client "Client"
   :scanner "Scanner"
   :control "Control"
   :trace "Trace"
   :status "Status"})

(def stats-kinds
  [:status :index :cron :clients :telemetry :counters])

(def stats-labels
  {:status "Status"
   :index "Index"
   :cron "Cron"
   :clients "Clients"
   :telemetry "Telemetry"
   :counters "Counters"})

(def query-modes
  [:semantic :text :grep])

(def query-labels
  {:semantic "Semantic"
   :text "Text"
   :grep "Grep"})

(def mandate-scopes
  [:all :global :workspace :project])

(def mandate-labels
  {:all "All"
   :global "Global"
   :workspace "Workspace"
   :project "Project"})

(def work-views
  [:next-actionable :needs-triage :blocked :overdue :my-work :all])

(def work-labels
  {:next-actionable "Next"
   :needs-triage "Triage"
   :blocked "Blocked"
   :overdue "Overdue"
   :my-work "Mine"
   :all "All"})

(def topic-set (set topics))
(def stats-kind-set (set stats-kinds))
(def query-mode-set (set query-modes))
(def mandate-scope-set (set mandate-scopes))
(def work-view-set (set work-views))

(defn normalize-keyword [value]
  (cond
    (keyword? value) value
    (nil? value) nil
    :else (keyword (str/lower-case (str/trim (str value))))))

(defn known-topic? [value]
  (contains? topic-set (normalize-keyword value)))

(defn normalize-topic [value]
  (let [topic (normalize-keyword value)]
    (if (known-topic? topic) topic value)))

(defn normalize-stats-kind [value]
  (let [kind (normalize-keyword value)]
    (if (contains? stats-kind-set kind) kind :status)))

(defn normalize-query-mode [value]
  (let [mode (normalize-keyword value)]
    (if (contains? query-mode-set mode) mode :semantic)))

(defn normalize-mandate-scope [value]
  (let [scope (normalize-keyword value)]
    (if (contains? mandate-scope-set scope) scope :all)))

(defn normalize-work-view [value]
  (let [view (normalize-keyword value)]
    (if (contains? work-view-set view) view :next-actionable)))

(defn choice [id labels]
  {:id id :label (get labels id (name id))})

(def stats-choices
  (mapv #(choice % stats-labels) stats-kinds))

(def query-mode-choices
  (mapv #(choice % query-labels) query-modes))

(def mandate-scope-choices
  (mapv #(choice % mandate-labels) mandate-scopes))

(def work-view-choices
  (mapv #(choice % work-labels) work-views))

(def work-kind-choices
  (mapv (fn [s] {:id s :label (if (= s "") "all kinds" s)})
        ["" "plan" "goal" "epic" "task" "sub_task" "todo" "fixme" "bug" "idea"
         "brainstorm" "note" "question" "nice_to_have" "action_item" "experiment"]))

(def work-status-choices
  (mapv (fn [s] {:id s :label (if (= s "") "all statuses" s)})
        ["" "pending" "triage" "confirmed" "ready" "in_progress" "blocked"
         "claimed_done" "verifying" "verified" "rejected" "deferred" "cancelled"]))

(defn topic-name [topic]
  (let [topic (normalize-topic topic)]
    (if (keyword? topic) (name topic) (str topic))))

(defn topic-label [topic]
  (let [topic (normalize-topic topic)]
    (get topic-labels topic (topic-name topic))))
