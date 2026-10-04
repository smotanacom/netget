"""pynetdicom 3.0.4, unchanged, as both ends of DICOM.

  peer.py scu HOST PORT   SCU against an SCP whose AE title is NETGET: a wrong called AE
                          title, a calling AE the service refuses, one it cannot decide on,
                          then C-ECHO, three C-STOREs (one refused), C-FINDs (wildcard, date
                          range, patient root, no match) and A-RELEASE. One JSON line per step.
  peer.py scp PORT        SCP titled PACS (called AE title required) for Verification, Study and
                          Patient Root C-FIND and three storage classes; prints each stored
                          instance as JSON and answers C-FIND from what it stored.
"""
import fnmatch, json, sys, time
from pydicom.dataset import Dataset, FileMetaDataset
from pydicom.uid import ExplicitVRLittleEndian
from pynetdicom import AE, evt
from pynetdicom.sop_class import (
    Verification, StudyRootQueryRetrieveInformationModelFind as STUDY_FIND,
    PatientRootQueryRetrieveInformationModelFind as PATIENT_FIND,
    SecondaryCaptureImageStorage as SC, CTImageStorage as CT, MRImageStorage as MR,
)

out = lambda **kw: print(json.dumps(kw), flush=True)


def instance(n, patient, pid, study, date):
    ds = Dataset()
    ds.file_meta = FileMetaDataset()
    ds.file_meta.TransferSyntaxUID = ExplicitVRLittleEndian
    ds.SOPClassUID = SC
    ds.SOPInstanceUID = f"1.2.826.0.1.3680043.10.1408.200.{n}"
    ds.StudyInstanceUID = study
    ds.SeriesInstanceUID = study + ".1"
    ds.PatientName = patient
    ds.PatientID = pid
    ds.StudyDate = date
    ds.Modality = "OT"
    ds.ConversionType = "WSD"
    ds.Rows, ds.Columns, ds.BitsAllocated, ds.SamplesPerPixel = 2, 2, 8, 1
    ds.PixelData = bytes([0, 1, 2, 3])
    return ds


def scu(host, port):
    def ae(calling="MODALITY"):
        a = AE(ae_title=calling)
        a.acse_timeout = a.dimse_timeout = a.network_timeout = 30
        for c in (Verification, STUDY_FIND, PATIENT_FIND, SC):
            a.add_requested_context(c)
        return a

    for step, calling, called in (("wrong_called", "MODALITY", "WRONG"), ("refused", "BLOCKED", "NETGET"), ("undecided", "BROKEN", "NETGET")):
        assoc = ae(calling).associate(host, port, ae_title=called)
        out(step=step, rejected=assoc.is_rejected, established=assoc.is_established)
        if assoc.is_established:
            assoc.release()
    assoc = ae().associate(host, port, ae_title="NETGET")
    out(step="associated", established=assoc.is_established,
        accepted=sorted(str(c.abstract_syntax) for c in assoc.accepted_contexts),
        transfer=sorted({str(c.transfer_syntax[0]) for c in assoc.accepted_contexts}))
    out(step="echo", status=assoc.send_c_echo().Status)
    for ds in (instance(1, "Doe^Jane", "P001", "1.2.826.0.1.3680043.10.1408.300.1", "20261002"),
               instance(2, "Roe^Rich", "P002", "1.2.826.0.1.3680043.10.1408.300.2", "20261015"),
               instance(3, "Moe^Mo", "REJECT", "1.2.826.0.1.3680043.10.1408.300.3", "20261020")):
        st = assoc.send_c_store(ds)
        out(step="store", pid=ds.PatientID, status=st.Status, comment=str(st.get("ErrorComment", "")))

    def find(step, model, **keys):
        q = Dataset()
        for k, v in keys.items():
            setattr(q, k, v)
        rows, final = [], None
        for status, ident in assoc.send_c_find(q, model):
            if status.Status in (0xFF00, 0xFF01):
                rows.append({k: str(ident.get(k, "")) for k in keys if k != "QueryRetrieveLevel"})
            else:
                final = status.Status
        out(step=step, final=final, rows=rows)

    find("find_wildcard", STUDY_FIND, QueryRetrieveLevel="STUDY", PatientName="*oe^*", StudyInstanceUID="", StudyDate="")
    find("find_range", STUDY_FIND, QueryRetrieveLevel="STUDY", StudyDate="20261010-20261031", PatientName="", PatientID="")
    find("find_patient", PATIENT_FIND, QueryRetrieveLevel="PATIENT", PatientID="P001", PatientName="")
    find("find_none", STUDY_FIND, QueryRetrieveLevel="STUDY", PatientName="Nobody", StudyInstanceUID="")
    assoc.release()
    out(step="released", released=assoc.is_released)


def scp(port):
    ae = AE(ae_title="PACS")
    ae.require_called_aet = True
    for c in (Verification, STUDY_FIND, PATIENT_FIND, SC, CT, MR):
        ae.add_supported_context(c)
    stored = []

    def on_store(event):
        ds = event.dataset
        stored.append(ds)
        out(stored=str(ds.SOPInstanceUID), sop_class=str(ds.SOPClassUID), patient=str(ds.PatientName),
            pid=str(ds.PatientID), pixels=bytes(ds.get("PixelData", b"")).hex(), calling=event.assoc.requestor.ae_title)
        return 0xA700 if ds.PatientID == "FULL" else 0x0000

    def on_find(event):
        q = event.identifier
        out(find=str(q.QueryRetrieveLevel), patient=str(q.get("PatientName", "")))
        pattern = str(q.get("PatientName", "")) or "*"
        for ds in stored:
            if fnmatch.fnmatchcase(str(ds.PatientName), pattern):
                r = Dataset()
                r.QueryRetrieveLevel = q.QueryRetrieveLevel
                r.PatientName = ds.PatientName
                r.PatientID = ds.PatientID
                r.StudyInstanceUID = ds.StudyInstanceUID
                yield 0xFF00, r

    ae.start_server(("127.0.0.1", port), block=False, evt_handlers=[(evt.EVT_C_STORE, on_store), (evt.EVT_C_FIND, on_find)])
    print(f"listening on {port}", flush=True)
    while True:
        time.sleep(3600)


if sys.argv[1] == "scu":
    scu(sys.argv[2], int(sys.argv[3]))
else:
    scp(int(sys.argv[2]))
