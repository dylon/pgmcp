(ns pgmcp.webui.views.markdown
  "Renders text as GitHub-Flavored Markdown. The unified/remark→hast→hiccup
  pipeline runs async in fx (:render/md); until it resolves the raw text shows
  in a bounded pre. Output is hiccup (never raw HTML), so it can carry real
  reagent structure and stays within the no-raw-HTML gate."
  (:require [reagent.core :as r]
            [re-frame.core :as rf]))

(defn markdown-view [id text]
  (r/with-let [_ (rf/dispatch [:render/md id (str text)])]
    (let [rendered @(rf/subscribe [:render/result id])]
      (if rendered
        [:div.markdown-body rendered]
        [:pre.markdown-pending (str text)]))))
