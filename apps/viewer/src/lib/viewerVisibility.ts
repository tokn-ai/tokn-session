import { createContext, useContext } from "react";

/** A connected viewer can keep reading while another in-app page has the screen. */
export const ViewerVisibility = createContext(true);
export function useViewerVisible() { return useContext(ViewerVisibility); }
