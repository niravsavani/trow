use failure::{format_err, Error};
use futures::{Future, Stream};
use grpcio::Channel;
use std::fs::OpenOptions;
use std::io::prelude::*;
use trow_protobuf::server::*;
use trow_protobuf::server_grpc::RegistryClient;
use trow_protobuf::server_grpc::AdmissionControllerClient;
use types::{self, *};

/* Will move to server grpc */
pub struct BackendClient {
    chan: Channel,
}

impl BackendClient {
    pub fn new(chan: Channel) -> BackendClient {
        BackendClient { chan }
    }
}

pub struct ClientInterface {
    rc: RegistryClient,
    ac: AdmissionControllerClient
}

impl ClientInterface {
    pub fn new(backend: BackendClient) -> Self {

        //Not sure if there's a reason we can't pass a reference to a channel
        let rc = RegistryClient::new(backend.chan.clone());
        let ac = AdmissionControllerClient::new(backend.chan);
        ClientInterface { rc, ac }
    }

    /**
     * Ok so these functions are largely boilerplate to call the GRPC backend.
     * But doing it here lets us change things behind the scenes much cleaner.
     *
     * Frontend code becomes smaller and doesn't need to know about GRPC types.
     * In fact you could pull it out for a different implementation now by
     * just changing this file...
     **/

    pub fn request_upload(&self, repo_name: &RepoName) -> Result<UploadInfo, Error> {
        let mut req = UploadRequest::new();
        req.set_repo_name(repo_name.0.clone());

        let response = self.rc.request_upload(&req)?;

        Ok(create_upload_info(
            types::Uuid(response.get_uuid().to_owned()),
            repo_name.clone(),
            (0, 0),
        ))
    }

    pub fn complete_upload(
        &self,
        repo_name: &RepoName,
        uuid: &Uuid,
        digest: &Digest,
    ) -> Result<AcceptedUpload, Error> {
        let mut req = CompleteRequest::new();
        req.set_repo_name(repo_name.0.clone());
        req.set_uuid(uuid.0.clone());
        req.set_user_digest(digest.0.clone());
        let resp = self.rc.complete_upload(&req)?;

        Ok(create_accepted_upload(
            Digest(resp.digest.to_owned()),
            repo_name.clone(),
        ))
    }

    pub fn get_write_sink_for_upload(
        &self,
        repo_name: &RepoName,
        uuid: &Uuid,
    ) -> Result<impl Write, Error> {
        let mut br = BlobRef::new();
        br.set_uuid(uuid.0.clone());
        br.set_repo_name(repo_name.0.clone());

        let resp = self.rc.get_write_location_for_blob(&br)?;

        //For the moment we know it's a file location
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(resp.path)?;
        Ok(file)
    }

    pub fn get_write_sink_for_manifest(
        &self,
        repo_name: &RepoName,
        reference: &str,
    ) -> Result<impl Write, Error> {
        let mut mr = ManifestRef::new();
        mr.set_reference(reference.to_owned());
        mr.set_repo_name(repo_name.0.clone());

        let resp = self.rc.get_write_location_for_manifest(&mr)?;

        //For the moment we know it's a file location
        //Manifests don't append; just overwrite
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(resp.path)?;
        Ok(file)
    }

    pub fn get_reader_for_manifest(
        &self,
        repo_name: &RepoName,
        reference: &str,
    ) -> Result<ManifestReader, Error> {
        let mut mr = ManifestRef::new();
        mr.set_reference(reference.to_owned());
        mr.set_repo_name(repo_name.0.clone());

        let resp = self.rc.get_read_location_for_manifest(&mr)?;

        //For the moment we know it's a file location
        let file = OpenOptions::new().read(true).open(resp.path)?;
        let mr = create_manifest_reader(
            Box::new(file),
            resp.content_type,
            Digest(resp.digest.to_owned()),
        );
        Ok(mr)
    }

    pub fn get_reader_for_blob(
        &self,
        repo_name: &RepoName,
        digest: &Digest,
    ) -> Result<BlobReader, Error> {
        let mut dr = DownloadRef::new();
        dr.set_digest(digest.0.clone());
        dr.set_repo_name(repo_name.0.clone());

        let resp = self.rc.get_read_location_for_blob(&dr)?;

        //For the moment we know it's a file location
        let file = OpenOptions::new().read(true).open(resp.path)?;
        let br = create_blob_reader(Box::new(file), digest.clone());
        Ok(br)
    }

    pub fn verify_manifest(
        &self,
        repo_name: &RepoName,
        reference: &str,
    ) -> Result<types::VerifiedManifest, Error> {
        let mut mr = ManifestRef::new();
        mr.set_reference(reference.to_owned());
        mr.set_repo_name(repo_name.0.clone());

        let resp = self.rc.verify_manifest(&mr)?;

        let vm = create_verified_manifest(
            repo_name.clone(),
            Digest(resp.get_digest().to_string()),
            reference.to_string(),
            resp.get_content_type().to_string(),
        );
        Ok(vm)
    }

    pub fn get_catalog(&self) -> Result<RepoCatalog, Error> {
        let cr = CatalogRequest::new();
        let mut repo_stream = self.rc.get_catalog(&cr)?;
        let mut catalog = RepoCatalog::new();

        loop {
            let f = repo_stream.into_future();
            match f.wait() {
                Ok((Some(ce), s)) => {
                    repo_stream = s;
                    catalog.insert(RepoName(ce.get_repo_name().to_string()));
                }
                Ok((None, _)) => break,
                Err((e, _)) => return Err(format_err!("Failure streaming from server {:?}", e)),
            }
        }

        Ok(catalog)
    }

    pub fn list_tags(&self, repo_name: &RepoName) -> Result<TagList, Error> {
        let mut ce = CatalogEntry::new();
        ce.set_repo_name(repo_name.0.clone());

        let mut tag_stream = self.rc.list_tags(&ce)?;
        let mut list = TagList::new(repo_name.clone());

        loop {
            let f = tag_stream.into_future();
            match f.wait() {
                Ok((Some(tag), s)) => {
                    tag_stream = s;
                    list.insert(tag.get_tag().to_string());
                }
                Ok((None, _)) => break,
                Err((e, _)) => return Err(format_err!("Failure streaming from server {:?}", e)),
            }
        }

        Ok(list)
    }

    /**
     * Ok result indicates admission was validated.
     */
    pub fn validate_admission(&self, a_rev: &AdmissionReview) 
    -> Result<(), Error> {
        
        //Should be able to write something to convert automatically
        let mut a_req = AdmissionRequest::new();
        a_req.set_api_version(a_rev.api_version.clone());
        a_req.set_uid(a_rev.uid.clone());
        a_req.set_image(a_rev.image.clone());
        a_req.set_namespace(a_rev.namespace.clone());
        a_req.set_operation(a_rev.operation.clone());

        let resp = self.ac.validate_admission(&a_req)?;

        if !resp.valid {
            return Err(format_err!("Failed validation: {}", resp.reason));
        }
        Ok(())
    }
}
