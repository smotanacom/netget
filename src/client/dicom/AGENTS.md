# DICOM client (SCU) — Experimental

Shares `src/server/dicom/pdu.rs` and `src/server/dicom/dataset.rs`. Connect proposes Verification, Study and Patient Root
C-FIND and `storage_classes` (CT, MR, Secondary Capture by default) with Explicit then Implicit
VR LE, as `calling_ae` to `called_ae`; a rejection fails the connect. `dicom_associated` lists the
accepted contexts. Actions: `dicom_echo`, `dicom_store` (DICOM JSON; binary values as `hex`,
SOP UIDs written into the dataset), `dicom_find` (pending identifiers gathered into `matches`) and
`disconnect` (A-RELEASE). Each answer is one `dicom_response` with the status as four hex digits
and its meaning.
