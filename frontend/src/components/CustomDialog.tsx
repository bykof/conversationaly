import React from "react";
import { Settings } from "lucide-react";
import { Dialog, DialogContent, DialogTitle, DialogTrigger, DialogFooter } from "./ui/dialog";
import { VisuallyHidden } from "./ui/visually-hidden";
import { SettingTabs } from "./SettingTabs";

interface DialogProps {
    triggerComponent: React.ReactElement;
    dialogContent: React.ReactNode;
    dialogTitle?: string;
}

export function CustomDialog({ triggerComponent, dialogContent, dialogTitle = "Dialog" }: DialogProps) {
    return (
        <Dialog>
            {/* asChild forwards the ref onto this element; cloning it with its
                own props back was a no-op, and React 19 types `props` as
                unknown, so the spread no longer compiles. */}
            <DialogTrigger asChild>
                {triggerComponent}
            </DialogTrigger>
            <DialogContent aria-describedby={undefined}>
                <VisuallyHidden>
                    <DialogTitle>{dialogTitle}</DialogTitle>
                </VisuallyHidden>
                {dialogContent}                  
                <DialogFooter>
                    
                </DialogFooter>
            </DialogContent>
        </Dialog>
    )
}