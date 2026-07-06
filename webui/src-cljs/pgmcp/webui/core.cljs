(ns pgmcp.webui.core
  (:require [pgmcp.webui.events]
            [pgmcp.webui.fx :as fx]
            [pgmcp.webui.subs]
            [pgmcp.webui.views.editor]
            [pgmcp.webui.views.shell :as shell]
            [re-frame.core :as rf]
            [re-frame.db :as rf-db]
            [reagent.dom.client :as rdc]))

(defonce root (atom nil))

(defn machine-snapshot []
  (.parse js/JSON
          (.stringify js/JSON (clj->js (:machine @rf-db/app-db)))))

(defn mount! []
  (let [container (.getElementById js/document "app")
        next-root (or @root (rdc/create-root container))]
    (reset! root next-root)
    (rdc/render next-root [shell/app-root])))

(defn boot! []
  (set! (.-__pgmcpWebuiMachine js/window) machine-snapshot)
  (rf/dispatch-sync [:app/init {:token (fx/remembered-token)
                                :theme (fx/remembered-theme)}])
  (mount!))

(defn ^:export init! []
  (if (= "loading" (.-readyState js/document))
    (.addEventListener js/document "DOMContentLoaded" boot!)
    (boot!)))
